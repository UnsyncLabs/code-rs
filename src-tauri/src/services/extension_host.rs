//! Extension host service.
//!
//! VS Code runs extensions in a separate Node.js process (the "extension
//! host"). Under Electron this was a forked Node process; here we spawn Node as
//! a child process via `tauri-plugin-shell`, connect it over a message channel
//! (IPC/stdio or a local socket), and bridge messages to the workbench.
//!
//! TODO:
//!   - Locate the Node runtime (bundled or from the `.nvmrc` version on PATH).
//!   - Launch `out/bootstrap-fork` / the extension host entrypoint.
//!   - Establish the MessagePort-style protocol the workbench expects.
//!   - Track the child handle in shared app state for clean shutdown.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct ExtensionHostOptions {
    /// Absolute path to the node executable to use.
    pub node_path: Option<String>,
    /// Entry script for the extension host (e.g. the bootstrap fork module).
    pub entrypoint: String,
    /// Extra arguments passed to node.
    pub args: Vec<String>,
    /// Environment overrides.
    pub env: Vec<(String, String)>,
}

/// Starts the Node extension host child process. Returns an opaque host id.
#[tauri::command]
pub async fn start_extension_host(options: ExtensionHostOptions) -> Result<u64, String> {
    // TODO: spawn node via tauri-plugin-shell, wire up stdio/IPC, store handle.
    let _ = options;
    Err("extension_host::start_extension_host not implemented".into())
}

/// Stops a previously started extension host.
#[tauri::command]
pub async fn stop_extension_host(id: u64) -> Result<(), String> {
    // TODO: signal graceful shutdown, then kill if it does not exit in time.
    let _ = id;
    Err("extension_host::stop_extension_host not implemented".into())
}
