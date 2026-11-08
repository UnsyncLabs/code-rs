//! Window management service.
//!
//! Thin wrappers over Tauri's window APIs so the workbench can control the
//! native shell (title, fullscreen, later: custom title bar, menus, zoom).
//! TODO: operate on the concrete `WebviewWindow` via the `AppHandle`.

/// Sets the native window title.
#[tauri::command]
pub async fn set_title(window: tauri::Window, title: String) -> Result<(), String> {
    // TODO: window.set_title(&title).map_err(|e| e.to_string())
    let _ = (window, title);
    Err("window::set_title not implemented".into())
}

/// Toggles fullscreen for the given window.
#[tauri::command]
pub async fn toggle_fullscreen(window: tauri::Window) -> Result<bool, String> {
    // TODO: read window.is_fullscreen(), invert via window.set_fullscreen().
    let _ = window;
    Err("window::toggle_fullscreen not implemented".into())
}
