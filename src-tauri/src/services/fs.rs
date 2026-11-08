//! Filesystem service.
//!
//! Replaces the Node `fs` access the Electron main process gave the workbench.
//! TODO: implement with `tokio::fs` / `std::fs`, map errors to a serializable
//! error type, and enforce workspace-scoped path validation.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct FileStat {
    pub is_file: bool,
    pub is_dir: bool,
    pub size: u64,
    /// Last modification time in milliseconds since the Unix epoch.
    pub mtime_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

/// Reads a file and returns its contents as a UTF-8 string.
#[tauri::command]
pub async fn read_file(path: String) -> Result<String, String> {
    // TODO: read bytes, support binary via base64, enforce path sandboxing.
    let _ = path;
    Err("fs::read_file not implemented".into())
}

/// Writes `contents` to `path`, creating or truncating the file.
#[tauri::command]
pub async fn write_file(path: String, contents: String) -> Result<(), String> {
    // TODO: atomic write (temp file + rename), create parent dirs as needed.
    let _ = (path, contents);
    Err("fs::write_file not implemented".into())
}

/// Returns metadata for a path.
#[tauri::command]
pub async fn stat(path: String) -> Result<FileStat, String> {
    // TODO: std::fs::metadata -> FileStat.
    let _ = path;
    Err("fs::stat not implemented".into())
}

/// Lists the entries of a directory.
#[tauri::command]
pub async fn read_dir(path: String) -> Result<Vec<DirEntry>, String> {
    // TODO: std::fs::read_dir -> Vec<DirEntry>.
    let _ = path;
    Err("fs::read_dir not implemented".into())
}
