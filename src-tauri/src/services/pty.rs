//! Pseudo-terminal (pty) service powering the integrated terminal.
//!
//! Replaces node-pty used by the Electron build. TODO: implement with the
//! `portable-pty` crate (ConPTY on Windows, forkpty on unix), stream output to
//! the frontend via Tauri events, and track sessions in shared state.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct PtySpawnOptions {
    pub shell: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub env: Vec<(String, String)>,
}

/// Spawns a new pty session and returns its id.
#[tauri::command]
pub async fn pty_spawn(options: PtySpawnOptions) -> Result<u64, String> {
    let _ = options;
    Err("pty::pty_spawn not implemented".into())
}

/// Writes input bytes (as a UTF-8 string) to the pty identified by `id`.
#[tauri::command]
pub async fn pty_write(id: u64, data: String) -> Result<(), String> {
    let _ = (id, data);
    Err("pty::pty_write not implemented".into())
}

/// Resizes the pty viewport.
#[tauri::command]
pub async fn pty_resize(id: u64, cols: u16, rows: u16) -> Result<(), String> {
    let _ = (id, cols, rows);
    Err("pty::pty_resize not implemented".into())
}

/// Terminates the pty session.
#[tauri::command]
pub async fn pty_kill(id: u64) -> Result<(), String> {
    let _ = id;
    Err("pty::pty_kill not implemented".into())
}
