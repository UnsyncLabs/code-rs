//! Workspace text snapshots taken before an external agent's turn, so the chat can
//! show what the turn changed (including edits made through the agent's own shell,
//! which never pass through `fs/write_text_file`).
//!
//! Bounded: skips heavy folders, binaries and big files, and stops at a total size.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

const SKIP_DIRS: &[&str] = &[
    ".git", "node_modules", "target", "out", "dist", "build", ".next", ".venv", "venv",
    "__pycache__", ".cache", ".gradle", ".idea",
];
const MAX_FILE: u64 = 512 * 1024;
const MAX_TOTAL: usize = 48 * 1024 * 1024;
const MAX_FILES: usize = 20_000;
/// Snapshots kept at most (older ones are dropped).
const MAX_SNAPSHOTS: usize = 8;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

type Snapshot = HashMap<PathBuf, String>;

fn store() -> &'static Mutex<Vec<(u64, Snapshot)>> {
    static S: OnceLock<Mutex<Vec<(u64, Snapshot)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Vec::new()))
}

fn collect(root: &Path) -> Snapshot {
    let mut out = Snapshot::new();
    let mut total = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                let name = entry.file_name();
                if !SKIP_DIRS.iter().any(|s| name.eq_ignore_ascii_case(s)) {
                    stack.push(path);
                }
            } else if ft.is_file() {
                if out.len() >= MAX_FILES || total >= MAX_TOTAL {
                    return out;
                }
                if entry.metadata().map(|m| m.len() > MAX_FILE).unwrap_or(true) {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else { continue };
                if bytes.contains(&0) {
                    continue; // binary
                }
                if let Ok(text) = String::from_utf8(bytes) {
                    total += text.len();
                    out.insert(path, text);
                }
            }
        }
    }
    out
}

fn key(p: &Path) -> PathBuf {
    // Case-insensitive, separator-normalized on Windows.
    if cfg!(windows) {
        PathBuf::from(p.to_string_lossy().replace('/', "\\").to_lowercase())
    } else {
        p.to_path_buf()
    }
}

/// Snapshot the text files under `root`; returns the snapshot id.
#[tauri::command]
pub async fn acp_snapshot(root: String) -> Result<u64, String> {
    let snap = tauri::async_runtime::spawn_blocking(move || {
        collect(Path::new(&root)).into_iter().map(|(p, t)| (key(&p), t)).collect::<Snapshot>()
    })
    .await
    .map_err(|e| e.to_string())?;
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    let mut s = store().lock().map_err(|e| e.to_string())?;
    s.push((id, snap));
    let excess = s.len().saturating_sub(MAX_SNAPSHOTS);
    s.drain(..excess);
    Ok(id)
}

/// The content of `path` in snapshot `id`: `None` when it did not exist (or was skipped).
#[tauri::command]
pub fn acp_snapshot_read(id: u64, path: String) -> Option<String> {
    let s = store().lock().ok()?;
    s.iter().find(|(i, _)| *i == id)?.1.get(&key(Path::new(&path))).cloned()
}

/// Forget a snapshot once the turn has been reported.
#[tauri::command]
pub fn acp_snapshot_drop(id: u64) {
    if let Ok(mut s) = store().lock() {
        s.retain(|(i, _)| *i != id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_text_and_skips_binaries_and_heavy_dirs() {
        let dir = std::env::temp_dir().join(format!("acp-snap-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        std::fs::write(dir.join("b.bin"), [0u8, 1, 2]).unwrap();
        std::fs::write(dir.join("node_modules").join("c.js"), "skip").unwrap();
        let snap = collect(&dir);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap.get(&dir.join("a.py")).map(String::as_str), Some("x = 1\n"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
