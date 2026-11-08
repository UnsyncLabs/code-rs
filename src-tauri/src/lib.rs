//! code-rs: a Tauri 2 shell that hosts the VS Code TypeScript web workbench,
//! replacing the Electron main/renderer processes.
//!
//! The frontend (the workbench) is the compiled web build served either by the
//! `@vscode/test-web` dev server (dev) or from the packaged `vscode-web` folder
//! (release). See `docs/rust-rewrite/TAURI_SETUP.md`.
//!
//! The Rust side exposes native services (filesystem, file watching, pty,
//! search, window, extension host) to the workbench via `tauri::command`
//! handlers. Those live under `services/` and are currently stubs.

pub mod services;

/// Builds and runs the Tauri application.
///
/// Marked `#[cfg_attr(mobile, tauri::mobile_entry_point)]` so the same entry
/// point can be reused if mobile targets are added later.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|_app| {
            // `manage` is a `Manager` trait method; bring it into scope locally
            // to keep the edit minimal and avoid a top-level import.
            use tauri::Manager as _;
            // Register search cancellation state so search_text/search_files
            // can look it up via `State<SearchState>` and `AppHandle::state`.
            _app.manage(services::search::SearchState::default());
            // TODO: initialize other native services here (start the extension
            // host, wire up watchers, etc.) once their stubs are implemented.
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // --- fs ---
            services::fs::read_file,
            services::fs::write_file,
            services::fs::stat,
            services::fs::read_dir,
            // --- watcher ---
            services::watcher::watch,
            services::watcher::unwatch,
            // --- pty ---
            services::pty::pty_spawn,
            services::pty::pty_write,
            services::pty::pty_resize,
            services::pty::pty_kill,
            // --- search ---
            services::search::search_text,
            services::search::search_files,
            services::search::search_cancel,
            // --- window ---
            services::window::set_title,
            services::window::toggle_fullscreen,
            // --- extension_host ---
            services::extension_host::start_extension_host,
            services::extension_host::stop_extension_host,
        ])
        .run(tauri::generate_context!())
        .expect("error while running code-rs tauri application");
}
