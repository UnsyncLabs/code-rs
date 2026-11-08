//! File-watching service.
//!
//! Replaces VS Code's Node-side file watcher (chokidar / parcel watcher /
//! native recursive watcher). TODO: implement with the `notify` crate and emit
//! change events back to the workbench via `AppHandle::emit`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct WatchOptions {
    pub recursive: bool,
    pub excludes: Vec<String>,
}

/// Starts watching `path`. Returns an opaque watcher id the frontend can use to
/// later `unwatch`. Change events are delivered out-of-band as Tauri events
/// (e.g. on a `"watcher://change"` channel). TODO.
#[tauri::command]
pub async fn watch(path: String, options: WatchOptions) -> Result<u64, String> {
    let _ = (path, options);
    Err("watcher::watch not implemented".into())
}

/// Stops a watcher previously created by [`watch`].
#[tauri::command]
pub async fn unwatch(id: u64) -> Result<(), String> {
    let _ = id;
    Err("watcher::unwatch not implemented".into())
}
