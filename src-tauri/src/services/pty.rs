//! Pseudo-terminal (pty) service powering the integrated terminal.
//!
//! Replaces `node-pty` used by the Electron build. Implemented with the
//! [`portable-pty`] crate (ConPTY on Windows, `forkpty`/`openpty` on unix).
//!
//! # Model
//! Each call to [`pty_spawn`] creates a `PtySession`, assigns it a monotonic
//! `u64` id, spawns the requested shell inside a freshly allocated pty, and
//! starts a background reader thread. That thread streams raw output to the
//! frontend over the Tauri event `pty://data/{id}` and, when the child exits,
//! emits `pty://exit/{id}` carrying the exit code. Sessions are tracked in a
//! process-wide [`PtyState`] so later `write`/`resize`/`kill`/`list` calls can
//! address them by id.
//!
//! # Events
//! - `pty://data/{id}` — payload is a UTF-8 `String` chunk of terminal output.
//! - `pty://exit/{id}`  — payload is [`PtyExitPayload`] with the exit code.
//!
//! The matching frontend consumer is
//! `src/vs/platform/terminal/browser/tauriPty.ts`.
//!
//! [`portable-pty`]: https://docs.rs/portable-pty

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Runtime, State};

/// Options accepted by [`pty_spawn`].
///
/// Mirrors the arguments the VS Code workbench passes when launching a terminal
/// process: `create(shell, args, cwd, env, cols, rows)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtySpawnOptions {
    /// Executable to launch. When `None`/empty the OS default shell is used
    /// (see [`default_shell`]).
    #[serde(default)]
    pub shell: Option<String>,
    /// Arguments passed to the shell.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory for the child. When `None` the process inherits the
    /// server's current directory.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Extra environment variables as `(key, value)` pairs. These are layered
    /// on top of the inherited environment.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Initial terminal width in columns.
    pub cols: u16,
    /// Initial terminal height in rows.
    pub rows: u16,
}

/// Payload emitted on `pty://exit/{id}` when a child process terminates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyExitPayload {
    pub id: u64,
    /// Platform exit code. `None` when it could not be determined (for example
    /// the process was killed by a signal on unix without a numeric status).
    pub exit_code: Option<i32>,
}

/// A description of a live pty session, returned by [`pty_list`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyProcessInfo {
    pub id: u64,
    /// OS process id of the shell, when known.
    pub pid: Option<u32>,
    /// The resolved shell executable that was launched.
    pub shell: String,
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
}

/// Internal per-session bookkeeping.
struct PtySession {
    /// Master side of the pty. Used for resize; kept alive so the pty is not
    /// torn down while the session exists.
    master: Box<dyn MasterPty + Send>,
    /// Writer into the pty (stdin of the child).
    writer: Box<dyn Write + Send>,
    /// Handle to the child process for signalling / killing.
    child: Box<dyn Child + Send + Sync>,
    pid: Option<u32>,
    shell: String,
    cwd: Option<String>,
    cols: u16,
    rows: u16,
}

/// Process-wide registry of pty sessions, managed by Tauri as shared state.
#[derive(Default)]
pub struct PtyState {
    sessions: Mutex<HashMap<u64, PtySession>>,
    next_id: AtomicU64,
}

impl PtyState {
    fn alloc_id(&self) -> u64 {
        // Start ids at 1 so `0` can retain its "no reconnection" meaning in the
        // terminal protocol.
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Returns the OS default shell executable.
///
/// - Windows: prefer PowerShell 7+ (`pwsh.exe`) if on `PATH`, otherwise Windows
///   PowerShell (`powershell.exe`); falls back to `cmd.exe`.
/// - Unix: the `SHELL` environment variable if set, otherwise `/usr/bin/bash`
///   (then `/bin/bash`, then `/bin/sh`).
pub fn default_shell() -> String {
    #[cfg(windows)]
    {
        if which_on_path("pwsh.exe") {
            return "pwsh.exe".to_string();
        }
        if which_on_path("powershell.exe") {
            return "powershell.exe".to_string();
        }
        // ComSpec is the canonical way to find cmd.exe.
        if let Ok(comspec) = std::env::var("ComSpec") {
            if !comspec.is_empty() {
                return comspec;
            }
        }
        "cmd.exe".to_string()
    }
    #[cfg(not(windows))]
    {
        if let Ok(shell) = std::env::var("SHELL") {
            if !shell.is_empty() {
                return shell;
            }
        }
        for candidate in ["/usr/bin/bash", "/bin/bash", "/bin/sh"] {
            if std::path::Path::new(candidate).exists() {
                return candidate.to_string();
            }
        }
        "/usr/bin/bash".to_string()
    }
}

/// Minimal `PATH` lookup used to pick between `pwsh`/`powershell` on Windows.
#[cfg(windows)]
fn which_on_path(exe: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(exe).is_file())
}

/// Builds a [`CommandBuilder`] from spawn options, resolving the default shell
/// when none is supplied.
fn build_command(options: &PtySpawnOptions) -> (CommandBuilder, String) {
    let shell = match options.shell.as_deref() {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => default_shell(),
    };

    let mut cmd = CommandBuilder::new(&shell);
    for arg in &options.args {
        cmd.arg(arg);
    }
    if let Some(cwd) = &options.cwd {
        if !cwd.is_empty() {
            cmd.cwd(cwd);
        }
    }
    for (key, value) in &options.env {
        cmd.env(key, value);
    }

    (cmd, shell)
}

/// Spawns a new pty session and returns its id.
///
/// Output is streamed asynchronously on `pty://data/{id}`; process exit is
/// reported on `pty://exit/{id}`.
#[tauri::command]
pub async fn pty_spawn<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, PtyState>,
    options: PtySpawnOptions,
) -> Result<u64, String> {
    let pty_system = portable_pty::native_pty_system();

    let size = PtySize {
        rows: options.rows.max(1),
        cols: options.cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    };

    let pair = pty_system
        .openpty(size)
        .map_err(|e| format!("failed to open pty: {e}"))?;

    let (cmd, shell) = build_command(&options);

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("failed to spawn '{shell}': {e}"))?;

    // The slave handle is no longer needed by this process once the child owns
    // it; dropping it avoids keeping the pty open after the child exits (which
    // would prevent the reader from observing EOF).
    drop(pair.slave);

    let pid = child.process_id();

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("failed to clone pty reader: {e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("failed to take pty writer: {e}"))?;

    let id = state.alloc_id();

    let session = PtySession {
        master: pair.master,
        writer,
        child,
        pid,
        shell: shell.clone(),
        cwd: options.cwd.clone(),
        cols: size.cols,
        rows: size.rows,
    };

    state
        .sessions
        .lock()
        .map_err(|_| "pty state mutex poisoned".to_string())?
        .insert(id, session);

    // Reader thread: stream output until EOF, then wait for the child and emit
    // the exit event. Uses a blocking read on a dedicated OS thread because the
    // pty reader is synchronous.
    let app_for_reader = app.clone();
    std::thread::Builder::new()
        .name(format!("pty-reader-{id}"))
        .spawn(move || read_loop(app_for_reader, id, reader))
        .map_err(|e| format!("failed to start pty reader thread: {e}"))?;

    Ok(id)
}

/// Blocking read loop executed on a dedicated thread per session.
fn read_loop<R: Runtime>(app: AppHandle<R>, id: u64, mut reader: Box<dyn Read + Send>) {
    let data_event = format!("pty://data/{id}");
    let mut buf = [0u8; 8192];

    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF: the child closed the pty.
            Ok(n) => {
                // Terminal output may split multi-byte UTF-8 sequences across
                // reads, so decode lossily to always produce a valid string.
                let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                // If emitting fails the frontend/window is gone; stop reading.
                if app.emit(&data_event, chunk).is_err() {
                    break;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    // Reap the child and report the exit code. The session is removed from the
    // registry so stale ids are not addressable afterwards.
    let exit_code = reap_child(&app, id);

    let _ = app.emit(
        &format!("pty://exit/{id}"),
        PtyExitPayload { id, exit_code },
    );
}

/// Waits for the child owned by session `id` (if still present) and returns its
/// numeric exit code. Removes the session from the registry.
fn reap_child<R: Runtime>(app: &AppHandle<R>, id: u64) -> Option<i32> {
    let state = app.state::<PtyState>();
    let mut session = {
        let mut sessions = match state.sessions.lock() {
            Ok(s) => s,
            Err(_) => return None,
        };
        sessions.remove(&id)
    }?;

    match session.child.wait() {
        Ok(status) => Some(status.exit_code() as i32),
        Err(_) => None,
    }
}

/// Writes input bytes (as a UTF-8 string) to the pty identified by `id`.
#[tauri::command]
pub async fn pty_write(state: State<'_, PtyState>, id: u64, data: String) -> Result<(), String> {
    let mut sessions = state
        .sessions
        .lock()
        .map_err(|_| "pty state mutex poisoned".to_string())?;
    let session = sessions
        .get_mut(&id)
        .ok_or_else(|| format!("pty {id} not found"))?;
    session
        .writer
        .write_all(data.as_bytes())
        .map_err(|e| format!("failed to write to pty {id}: {e}"))?;
    session
        .writer
        .flush()
        .map_err(|e| format!("failed to flush pty {id}: {e}"))?;
    Ok(())
}

/// Resizes the pty viewport.
#[tauri::command]
pub async fn pty_resize(
    state: State<'_, PtyState>,
    id: u64,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let mut sessions = state
        .sessions
        .lock()
        .map_err(|_| "pty state mutex poisoned".to_string())?;
    let session = sessions
        .get_mut(&id)
        .ok_or_else(|| format!("pty {id} not found"))?;
    let cols = cols.max(1);
    let rows = rows.max(1);
    session
        .master
        .resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("failed to resize pty {id}: {e}"))?;
    session.cols = cols;
    session.rows = rows;
    Ok(())
}

/// Terminates the pty session.
///
/// Kills the child process and drops the session; the reader thread will then
/// observe EOF and emit `pty://exit/{id}`.
#[tauri::command]
pub async fn pty_kill(state: State<'_, PtyState>, id: u64) -> Result<(), String> {
    let mut sessions = state
        .sessions
        .lock()
        .map_err(|_| "pty state mutex poisoned".to_string())?;
    let session = sessions
        .get_mut(&id)
        .ok_or_else(|| format!("pty {id} not found"))?;
    session
        .child
        .kill()
        .map_err(|e| format!("failed to kill pty {id}: {e}"))?;
    // Keep the session in the map; the reader thread reaps it and emits the
    // exit event once the pty reaches EOF, matching the natural-exit path.
    Ok(())
}

/// Lists the currently tracked pty sessions.
#[tauri::command]
pub async fn pty_list(state: State<'_, PtyState>) -> Result<Vec<PtyProcessInfo>, String> {
    let sessions = state
        .sessions
        .lock()
        .map_err(|_| "pty state mutex poisoned".to_string())?;
    let mut list: Vec<PtyProcessInfo> = sessions
        .iter()
        .map(|(id, s)| PtyProcessInfo {
            id: *id,
            pid: s.pid,
            shell: s.shell.clone(),
            cwd: s.cwd.clone(),
            cols: s.cols,
            rows: s.rows,
        })
        .collect();
    list.sort_by_key(|p| p.id);
    Ok(list)
}
