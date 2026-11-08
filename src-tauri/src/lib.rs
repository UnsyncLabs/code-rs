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
        .setup(|app| {
            // Register filesystem watcher state so `watcher::watch`/`unwatch`
            // can track live debouncers. Other services add their own setup.
            services::watcher::init(app.handle());
            // TODO: initialize other native services here (start the extension
            // host, etc.) once the remaining stubs under `services/` are
            // implemented.
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // --- fs ---
            services::fs::read_file,
            services::fs::write_file,
            services::fs::stat,
            services::fs::read_dir,
            services::fs::mkdir,
            services::fs::delete,
            services::fs::rename,
            services::fs::copy,
            services::fs::realpath,
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
