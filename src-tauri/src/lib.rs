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

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `code-rs --mcp`: run as a stdio MCP bridge to the running instance instead of
/// opening a window. Returns the process exit code.
pub fn mcp_stdio_bridge() -> i32 {
    services::mcp::stdio_bridge()
}

/// Builds and runs the Tauri application.
///
/// Marked `#[cfg_attr(mobile, tauri::mobile_entry_point)]` so the same entry
/// point can be reused if mobile targets are added later.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        // Remembers size, position and maximized state. Visibility is not restored: the
        // window stays hidden until the first page has loaded (see below).
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::all()
                        & !tauri_plugin_window_state::StateFlags::VISIBLE,
                )
                .build(),
        )
        // Serves the workbench (out/, extensions/) from disk: no Node web server. Async so
        // file reads run off the UI thread (a sync handler blocks the webview on every file).
        .register_asynchronous_uri_scheme_protocol(
            services::app_protocol::SCHEME,
            |_ctx, request, responder| {
                tauri::async_runtime::spawn_blocking(move || {
                    responder.respond(services::app_protocol::handle(request))
                });
            },
        )
        // pty service (services/pty.rs) tracks live terminal sessions here.
        .manage(services::pty::PtyState::default())
        // Node extension host bridge state (services/extension_host.rs).
        .manage(services::extension_host::ExtensionHostState::default())
        // ACP client sessions (services/acp.rs).
        .manage(services::acp::AcpState::default())
        // Muse gadget service state (services/muse.rs).
        .manage(std::sync::Arc::new(services::muse::MuseState::default()))
        .setup(|app| {
            use tauri::Manager as _;
            // Where out/, extensions/ and node_modules/ live (repo in dev, bundle in release).
            services::app_protocol::init_app_root(app.handle());
            // Search cancellation state (services/search.rs).
            app.manage(services::search::SearchState::default());
            // Filesystem watcher state (services/watcher.rs).
            services::watcher::init(app.handle());

            // Muse: auto-connect the gadget link at startup when already paired.
            services::muse::init_autoconnect(app.handle());

            // Main window. Optional first CLI arg = folder to open.
            let mut url = services::app_protocol::start_url();
            if let Some(folder) = std::env::args().nth(1).filter(|a| !a.is_empty() && !a.starts_with('-')) {
                let folder = std::fs::canonicalize(&folder)
                    .map(|p| p.to_string_lossy().trim_start_matches(r"\\?\").to_string())
                    .unwrap_or(folder);
                url.push_str("?tauriFolder=");
                url.push_str(&urlencode(&folder));
            }
            tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External(url.parse().expect("valid start url")),
            )
            .title("code-rs")
            // Windows: the workbench draws the title bar and window controls.
            .decorations(!services::window::custom_frame())
            // Paint with the last theme background while pages (re)load: no white flash.
            .background_color(
                services::window::parse_color(&services::window::saved_background(app.handle()))
                    .unwrap_or(tauri::window::Color(255, 255, 255, 255)),
            )
            .initialization_script(&services::window::background_init_script(
                &services::window::saved_background(app.handle()),
            ))
            .inner_size(1200.0, 800.0)
            .min_inner_size(600.0, 400.0)
            // Hidden until the page has loaded (theme background + splash), so the user
            // never sees an empty or white window.
            .visible(false)
            .on_page_load(|webview, payload| {
                if payload.event() == tauri::webview::PageLoadEvent::Finished {
                    let _ = webview.show();
                }
            })
            .build()?;
            // Safety net: show the window even if the page never finishes loading.
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(3));
                if let Some(w) = handle.get_webview_window("main") {
                    if !w.is_visible().unwrap_or(true) {
                        let _ = w.show();
                    }
                }
            });
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
            services::pty::pty_list,
            // --- search ---
            services::search::search_text,
            services::search::search_files,
            services::search::search_cancel,
            // --- window ---
            services::window::set_title,
            services::window::toggle_fullscreen,
            services::window::set_theme_background,
            services::window::window_minimize,
            services::window::window_toggle_maximize,
            services::window::window_close,
            services::window::window_is_maximized,
            services::window::window_start_drag,
            services::window::window_has_custom_frame,
            // --- extension_host ---
            services::extension_host::start_extension_host,
            services::extension_host::stop_extension_host,
            services::extension_host::exthost_write,
            services::extension_host::exthost_paths,
            services::extension_install::extension_quarantine_vsix,
            services::extension_install::extension_quarantine_commit,
            services::extension_install::extension_quarantine_discard,
            services::extension_install::extension_uninstall_folder,
            services::extension_install::host_target_platform,
            // --- updates ---
            services::updater::updater_info,
            services::updater::updater_check,
            services::updater::updater_install,
            services::updater::updater_open_releases,
            // --- MCP server ---
            services::mcp::mcp_set_enabled,
            services::mcp::mcp_respond,
            services::mcp::mcp_emit_event,
            services::mcp::mcp_tools,
            services::mcp::mcp_status,
            services::mcp::mcp_recent_activity,
            // --- assistant (AI providers) ---
            services::assistant::assistant_providers,
            services::assistant::assistant_provider_save,
            services::assistant::assistant_provider_delete,
            services::assistant::assistant_complete,
            services::assistant::assistant_chat,
            // --- command guardian ---
            services::guardian::guardian_assess,
            services::guardian::guardian_exec,
            // --- ACP client (services/acp.rs) ---
            services::acp::acp_agents,
            services::acp::acp_start,
            services::acp::acp_prompt,
            services::acp::acp_cancel,
            services::acp::acp_stop,
            services::acp::acp_permission_result,
            services::acp::acp_fs_result,
            services::acp::acp_set_mode,
            services::acp_snapshot::acp_snapshot,
            services::acp_snapshot::acp_snapshot_read,
            services::acp_snapshot::acp_snapshot_drop,
            // --- muse gadget (Contract M) ---
            services::muse::muse_status,
            services::muse::muse_pair,
            services::muse::muse_unpair,
            services::muse::muse_connect,
            services::muse::muse_set_commands,
            services::muse::muse_send,
            services::muse::muse_command_result,
        ])
        .build(tauri::generate_context!())
        .expect("error while building code-rs tauri application")
        .run(|_app, event| {
            if let tauri::RunEvent::Exit = event {
                services::mcp::on_exit();
                services::acp::on_exit(_app);
            }
        });
}
