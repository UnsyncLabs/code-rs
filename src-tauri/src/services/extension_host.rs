//! Extension host service.
//!
//! VS Code runs extensions in a separate Node.js process (the "extension
//! host"). Under Electron this was launched as a *utility process* (see
//! `src/vs/platform/extensions/electron-main/extensionHostStarter.ts` and
//! `src/vs/workbench/services/extensions/electron-browser/localProcessExtensionHost.ts`).
//!
//! The extension host entry point is `out/bootstrap-fork.js`, invoked with the
//! env var `VSCODE_ESM_ENTRYPOINT=vs/workbench/api/node/extensionHostProcess`.
//! It talks the VS Code IPC *wire* protocol (`PersistentProtocol`, a 13-byte
//! header + payload framing) over a transport selected via environment:
//!
//!   * `VSCODE_EXTHOST_IPC_HOOK`          -> connect() to a named pipe / UDS
//!   * `VSCODE_EXTHOST_WILL_SEND_SOCKET`  -> receive a socket over node IPC
//!   * `VSCODE_WILL_SEND_MESSAGE_PORT`    -> receive an Electron MessagePort
//!
//! Electron used the MessagePort transport. Tauri has no MessagePort bridge to
//! a child process, so we use the **named pipe / unix domain socket** transport
//! (`VSCODE_EXTHOST_IPC_HOOK`): the extension host process does
//! `net.createConnection(pipeName)` and wraps the raw socket in a
//! `PersistentProtocol`. See `extensionHostProcess.ts::_createExtHostProtocol`,
//! the `ExtHostConnectionType.IPC` branch.
//!
//! This module therefore implements a **dumb byte relay**:
//!
//! ```text
//!   webview (PersistentProtocol over tauriExtensionHost ISocket)
//!        |  exthost_write(id, base64)        ^  exthost://data/{id}
//!        v                                   |
//!   ┌─────────────────────── Rust bridge ───────────────────────┐
//!   │  listener (named pipe / UDS at VSCODE_EXTHOST_IPC_HOOK)    │
//!   └───────────────────────────────────────────────────────────┘
//!        |  raw bytes                        ^  raw bytes
//!        v                                   |
//!   node child: bootstrap-fork.js -> extensionHostProcess
//! ```
//!
//! The framing is owned by the two `PersistentProtocol` endpoints (one in the
//! Node process, one in the webview). The bridge never parses it; it only
//! moves bytes and surfaces lifecycle (`started`/`exit`) events.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Runtime, State};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};

// Some of these are only used on one platform (namespaced pipes on Windows vs
// file-path domain sockets on unix); silence cross-platform unused warnings.
#[allow(unused_imports)]
use interprocess::local_socket::{
    tokio::prelude::*, GenericFilePath, GenericNamespaced, ListenerOptions, Name,
};

/// Options forwarded from the workbench to launch the Node extension host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionHostOptions {
    /// Absolute path to the node executable. When `None` we resolve it from
    /// `VSCODE_NODE`, then a bundled location, then `PATH`.
    #[serde(default)]
    pub node_path: Option<String>,

    /// Absolute path to `bootstrap-fork.js`. When `None` we resolve it relative
    /// to the app resource dir (`out/bootstrap-fork.js`).
    #[serde(default)]
    pub bootstrap_path: Option<String>,

    /// Extra arguments passed to the extension host entry (after
    /// `bootstrap-fork.js`), e.g. `--skipWorkspaceStorageLock`.
    #[serde(default)]
    pub args: Vec<String>,

    /// Extra node runtime flags (`execArgv`), e.g. `--inspect-port=0`.
    #[serde(default)]
    pub exec_argv: Vec<String>,

    /// Environment overrides merged on top of the current process environment.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// Payload emitted to the webview when the host finishes spawning.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartedPayload {
    pub id: u64,
    pub pid: Option<u32>,
}

/// Payload emitted to the webview when the host process exits.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitPayload {
    pub id: u64,
    pub code: Option<i32>,
    pub signal: Option<String>,
}

/// A control message sent to a host's supervisor task.
enum HostControl {
    /// Forward these raw bytes (webview -> Node socket).
    Write(Vec<u8>),
    /// Kill the child process and tear everything down.
    Kill,
}

/// The per-host handle stored in shared state. We deliberately keep the
/// `Child` *out* of shared state: it is owned by the supervisor task
/// ([`run_host`]) so it can be `await`ed for exit without holding a lock.
struct HostHandle {
    control: mpsc::UnboundedSender<HostControl>,
    /// OS process id, retained for diagnostics/inspection.
    #[allow(dead_code)]
    pid: Option<u32>,
}

/// Shared state registered with Tauri via `app.manage(ExtensionHostState::default())`.
#[derive(Default)]
pub struct ExtensionHostState {
    hosts: Mutex<HashMap<u64, HostHandle>>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Event name carrying raw bytes (base64) from the Node process to the webview.
fn data_event(id: u64) -> String {
    format!("exthost://data/{id}")
}
/// Event fired once the host process has spawned.
fn started_event(id: u64) -> String {
    format!("exthost://started/{id}")
}
/// Event fired when the host process exits.
fn exit_event(id: u64) -> String {
    format!("exthost://exit/{id}")
}

/// Resolve the Node executable path.
///
/// Order mirrors what a packaged VS Code-like app can rely on:
///   1. explicit `node_path` option,
///   2. `VSCODE_NODE` environment variable,
///   3. a bundled runtime under the app resource dir,
///   4. `node`/`node.exe` discovered on `PATH`.
fn resolve_node_path<R: Runtime>(
    app: &AppHandle<R>,
    opt: &ExtensionHostOptions,
) -> Result<PathBuf, String> {
    if let Some(p) = &opt.node_path {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
        return Err(format!("nodePath does not exist: {}", p.display()));
    }

    if let Ok(env_node) = std::env::var("VSCODE_NODE") {
        let p = PathBuf::from(env_node);
        if p.exists() {
            return Ok(p);
        }
    }

    // Bundled runtime: <resourceDir>/node[.exe] — convention only; optional.
    if let Ok(resource_dir) = app.path().resource_dir() {
        let bundled = resource_dir.join(node_bin_name());
        if bundled.exists() {
            return Ok(bundled);
        }
    }

    // Fall back to PATH lookup.
    which_on_path(node_bin_name())
        .ok_or_else(|| "could not locate the 'node' executable (set VSCODE_NODE or nodePath)".into())
}

fn node_bin_name() -> &'static str {
    if cfg!(windows) {
        "node.exe"
    } else {
        "node"
    }
}

/// Minimal cross-platform `which`: scans `PATH` for an executable by name.
fn which_on_path(bin: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Resolve `bootstrap-fork.js`.
fn resolve_bootstrap<R: Runtime>(
    app: &AppHandle<R>,
    opt: &ExtensionHostOptions,
) -> Result<PathBuf, String> {
    if let Some(p) = &opt.bootstrap_path {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
        return Err(format!("bootstrapPath does not exist: {}", p.display()));
    }

    // Look for out/bootstrap-fork.js relative to the resource dir, then cwd.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(resource_dir) = app.path().resource_dir() {
        candidates.push(resource_dir.join("out").join("bootstrap-fork.js"));
        candidates.push(resource_dir.join("bootstrap-fork.js"));
    }
    // Explicit app root (e.g. the repo checkout during development).
    if let Ok(root) = std::env::var("VSCODE_APP_ROOT") {
        candidates.push(PathBuf::from(root).join("out").join("bootstrap-fork.js"));
    }
    // Dev builds: the crate lives in <repo>/src-tauri, so also check the repo root.
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("out")
            .join("bootstrap-fork.js"),
    );
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("out").join("bootstrap-fork.js"));
        if let Some(parent) = cwd.parent() {
            candidates.push(parent.join("out").join("bootstrap-fork.js"));
        }
    }
    for c in candidates {
        if c.exists() {
            return Ok(c);
        }
    }
    Err("could not locate out/bootstrap-fork.js (pass bootstrapPath)".into())
}

/// Build a unique, platform-appropriate pipe/socket name for the IPC hook.
///
/// Returns `(hook, ns)` where:
///   * `hook` is advertised to Node via `VSCODE_EXTHOST_IPC_HOOK` and is what
///     `net.createConnection(hook)` connects to:
///       - Windows: a named pipe path `\\.\pipe\<name>`
///       - unix:    a filesystem UDS path `<tmp>/<name>.sock`
///   * `ns` is the string we turn into an `interprocess` `Name` to bind the
///     listener. On Windows that is the bare pipe name (namespaced); on unix it
///     is the same filesystem path as `hook` (file-backed).
fn make_ipc_hook(id: u64) -> (String, String) {
    let pid = std::process::id();
    let unique = format!("vscode-exthost-{pid}-{id}");
    if cfg!(windows) {
        let hook = format!(r"\\.\pipe\{unique}");
        (hook, unique)
    } else {
        let mut dir = std::env::temp_dir();
        dir.push(format!("{unique}.sock"));
        let hook = dir.to_string_lossy().into_owned();
        (hook.clone(), hook)
    }
}

/// Turn the `ns` string from [`make_ipc_hook`] into an `interprocess` `Name`,
/// using the namespaced scheme on Windows (named pipes) and the file-path
/// scheme on unix (domain sockets) so it matches what Node connects to.
fn ipc_name(ns: String) -> Result<Name<'static>, String> {
    #[cfg(windows)]
    {
        ns.to_ns_name::<GenericNamespaced>()
            .map_err(|e| format!("invalid ipc name: {e}"))
    }
    #[cfg(not(windows))]
    {
        ns.to_fs_name::<GenericFilePath>()
            .map_err(|e| format!("invalid ipc path: {e}"))
    }
}

/// Start the Node extension host and the byte relay.
///
/// Returns the opaque host id. The webview should subscribe to
/// `exthost://data/{id}`, `exthost://started/{id}` and `exthost://exit/{id}`
/// and drive a `PersistentProtocol` over the relay.
#[tauri::command]
pub async fn start_extension_host<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, ExtensionHostState>,
    options: ExtensionHostOptions,
) -> Result<u64, String> {
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);

    let node = resolve_node_path(&app, &options)?;
    let bootstrap = resolve_bootstrap(&app, &options)?;
    let (ipc_hook, ns_name) = make_ipc_hook(id);

    // Best-effort cleanup of a stale unix socket file from a previous run.
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&ns_name);
    }

    // 1) Bind the listener *before* spawning Node so connect() cannot race.
    let name = ipc_name(ns_name.clone())?;
    let listener = ListenerOptions::new()
        .name(name)
        .create_tokio()
        .map_err(|e| format!("failed to bind exthost ipc listener: {e}"))?;

    // 2) Build the Node command.
    //    argv: node <execArgv...> bootstrap-fork.js <args...>
    let mut cmd = Command::new(&node);
    cmd.args(&options.exec_argv)
        .arg(&bootstrap)
        .args(&options.args)
        .env(
            "VSCODE_ESM_ENTRYPOINT",
            "vs/workbench/api/node/extensionHostProcess",
        )
        // Select the IPC (named pipe / UDS) transport (IPCExtHostConnection).
        .env("VSCODE_EXTHOST_IPC_HOOK", &ipc_hook)
        .env("VSCODE_HANDLES_UNCAUGHT_ERRORS", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Caller-provided env overrides win.
    for (k, v) in &options.env {
        cmd.env(k, v);
    }

    // On Windows, detach into a new process group so a shell crash does not
    // brutally tear down the extension host (mirrors `detached: isWindows`).
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn node extension host: {e}"))?;
    let pid = child.id();

    // Surface child stdout/stderr through the log for diagnosability.
    if let Some(stdout) = child.stdout.take() {
        spawn_output_pump(stdout, false);
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_output_pump(stderr, true);
    }

    // 3) Control channel: webview writes + kill requests funnel here.
    let (control_tx, control_rx) = mpsc::unbounded_channel::<HostControl>();

    // 4) Register the handle before spawning the supervisor so that an
    //    immediately-following `exthost_write` finds it.
    {
        let mut hosts = state.hosts.lock().await;
        hosts.insert(
            id,
            HostHandle {
                control: control_tx,
                pid,
            },
        );
    }

    // 5) Supervise: own the child, accept the pipe connection, relay bytes,
    //    await exit, clean up state, emit `exit`.
    let app_sup = app.clone();
    tauri::async_runtime::spawn(async move {
        run_host(app_sup, id, child, listener, control_rx, ns_name).await;
    });

    // 6) Tell the webview we are up.
    let _ = app.emit(&started_event(id), StartedPayload { id, pid });

    Ok(id)
}

/// Write bytes (base64-encoded) from the webview to the extension host.
///
/// Called by `tauriExtensionHost.ts`'s `ISocket.write`. Bytes are the raw
/// `PersistentProtocol` wire output and are forwarded verbatim.
#[tauri::command]
pub async fn exthost_write(
    state: State<'_, ExtensionHostState>,
    id: u64,
    bytes: String,
) -> Result<(), String> {
    let decoded = BASE64
        .decode(bytes.as_bytes())
        .map_err(|e| format!("invalid base64: {e}"))?;
    let hosts = state.hosts.lock().await;
    let handle = hosts
        .get(&id)
        .ok_or_else(|| format!("unknown extension host {id}"))?;
    handle
        .control
        .send(HostControl::Write(decoded))
        .map_err(|_| format!("extension host {id} is gone"))
}

/// Stop a previously started extension host (graceful: close the socket write
/// side and kill the child; the supervisor emits the final `exit` event).
#[tauri::command]
pub async fn stop_extension_host(
    state: State<'_, ExtensionHostState>,
    id: u64,
) -> Result<(), String> {
    let hosts = state.hosts.lock().await;
    if let Some(handle) = hosts.get(&id) {
        let _ = handle.control.send(HostControl::Kill);
    }
    Ok(())
}

/// Supervisor task for a single extension host. Owns the `Child` so it can be
/// awaited for exit, owns the accepted socket, and relays bytes both ways.
async fn run_host<R: Runtime>(
    app: AppHandle<R>,
    id: u64,
    mut child: tokio::process::Child,
    listener: interprocess::local_socket::tokio::Listener,
    mut control_rx: mpsc::UnboundedReceiver<HostControl>,
    ns_name: String,
) {
    // Accept the single connection the extension host makes, but bail out early
    // if the process dies before it connects. We intentionally do *not* drain
    // the control channel here: a `Write` could arrive before the pipe is up
    // and recv() would consume (lose) it. The webview only writes after the
    // protocol handshake, which starts after the socket is connected, so in
    // practice nothing is sent this early.
    let stream = tokio::select! {
        accepted = listener.accept() => match accepted {
            Ok(s) => Some(s),
            Err(e) => {
                log::error!("exthost {id}: accept failed: {e}");
                None
            }
        },
        status = child.wait() => {
            log::error!("exthost {id}: process exited before connecting: {status:?}");
            eprintln!("exthost {id}: process exited before connecting: {status:?}");
            None
        }
    };

    if let Some(stream) = stream {
        // `Stream::split()` is deprecated in interprocess >= 2.4.7; use the
        // generic tokio splitter, which works because `Stream: AsyncRead +
        // AsyncWrite`.
        let (mut reader, mut writer) = tokio::io::split(stream);

        // socket -> webview
        let app_read = app.clone();
        let read_task = async move {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break, // EOF: Node closed the socket
                    Ok(n) => {
                        let encoded = BASE64.encode(&buf[..n]);
                        if app_read.emit(&data_event(id), encoded).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        log::debug!("exthost {id}: socket read error: {e}");
                        break;
                    }
                }
            }
        };

        // webview -> socket, plus kill handling
        let write_task = async move {
            while let Some(msg) = control_rx.recv().await {
                match msg {
                    HostControl::Write(chunk) => {
                        if writer.write_all(&chunk).await.is_err() {
                            break;
                        }
                        if writer.flush().await.is_err() {
                            break;
                        }
                    }
                    HostControl::Kill => break,
                }
            }
            let _ = writer.shutdown().await;
        };

        // Relay until the socket closes, the webview stops, or Kill arrives.
        tokio::select! {
            _ = read_task => {},
            _ = write_task => {},
        }
    }

    // Ensure the process is going down, then await its real exit.
    let _ = child.start_kill();
    let status = child.wait().await;
    let (code, signal) = match status {
        Ok(s) => (s.code(), exit_signal(&s)),
        Err(_) => (None, None),
    };

    // Remove our handle from shared state.
    if let Some(state) = app.try_state::<ExtensionHostState>() {
        let mut hosts = state.hosts.lock().await;
        hosts.remove(&id);
    }

    // Best-effort cleanup of the unix socket file.
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&ns_name);
    }
    #[cfg(not(unix))]
    {
        let _ = &ns_name;
    }

    let _ = app.emit(&exit_event(id), ExitPayload { id, code, signal: signal.clone() });
    log::info!("exthost {id}: exited (code={code:?}, signal={signal:?})");
    eprintln!("exthost {id}: exited (code={code:?}, signal={signal:?})");
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|s| s.to_string())
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<String> {
    None
}

/// Pump a child stdout/stderr stream to the log.
fn spawn_output_pump<S>(mut stream: S, is_err: bool)
where
    S: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tauri::async_runtime::spawn(async move {
        let mut buf = vec![0u8; 8 * 1024];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    let text = text.trim_end();
                    if text.is_empty() {
                        continue;
                    }
                    if is_err {
                        log::error!("[exthost stderr] {text}");
                        eprintln!("[exthost stderr] {text}");
                    } else {
                        log::info!("[exthost stdout] {text}");
                        eprintln!("[exthost stdout] {text}");
                    }
                }
                Err(_) => break,
            }
        }
    });
}

/// Filesystem locations the Node extension host needs (it cannot use the
/// workbench's `vscode-app://` URLs).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionHostPaths {
    /// Root containing `out/` and `extensions/`.
    pub app_root: String,
    /// Directory for extension host logs.
    pub logs_dir: String,
}

#[tauri::command]
pub fn exthost_paths<R: Runtime>(app: AppHandle<R>) -> Result<ExtensionHostPaths, String> {
    let app_root = super::app_protocol::app_root();
    let logs_dir = app
        .path()
        .app_log_dir()
        .map_err(|e| e.to_string())?
        .join("exthost");
    std::fs::create_dir_all(&logs_dir).map_err(|e| e.to_string())?;
    Ok(ExtensionHostPaths {
        app_root: app_root.to_string_lossy().into_owned(),
        logs_dir: logs_dir.to_string_lossy().into_owned(),
    })
}
