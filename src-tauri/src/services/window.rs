//! Window management service.
//!
//! Thin wrappers over Tauri's window APIs so the workbench can control the
//! native shell (title, fullscreen, background color).

use std::path::PathBuf;

use tauri::{window::Color, AppHandle, Manager, Runtime};

/// Default background before any theme was ever applied (VS Code "Light Modern").
const DEFAULT_BACKGROUND: &str = "#ffffff";
const BACKGROUND_FILE: &str = "theme-background.txt";

/// Sets the native window title.
#[tauri::command]
pub async fn set_title(window: tauri::Window, title: String) -> Result<(), String> {
    window.set_title(&title).map_err(|e| e.to_string())
}

/// Toggles fullscreen for the given window, returning the new state.
#[tauri::command]
pub async fn toggle_fullscreen(window: tauri::Window) -> Result<bool, String> {
    let next = !window.is_fullscreen().map_err(|e| e.to_string())?;
    window.set_fullscreen(next).map_err(|e| e.to_string())?;
    Ok(next)
}

/// Remembers the theme's editor background and applies it to the native
/// window, so reloads (open/close folder) never flash white.
#[tauri::command]
pub async fn set_theme_background<R: Runtime>(
    app: AppHandle<R>,
    window: tauri::WebviewWindow<R>,
    color: String,
) -> Result<(), String> {
    let parsed = parse_color(&color).ok_or_else(|| format!("invalid color: {color}"))?;
    window.set_background_color(Some(parsed)).map_err(|e| e.to_string())?;
    if let Some(path) = background_file(&app) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(path, &color).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Saved theme background (or the default) as a `#rrggbb[aa]` string.
pub fn saved_background<R: Runtime>(app: &AppHandle<R>) -> String {
    background_file(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| parse_color(s).is_some())
        .unwrap_or_else(|| DEFAULT_BACKGROUND.to_string())
}

/// Script injected before every page load: paints the page with the last
/// theme background immediately, before the workbench CSS arrives.
pub fn background_init_script(saved: &str) -> String {
    format!(
        r#"(function(){{try{{var c=localStorage.getItem('code-rs.themeBackground')||'{saved}';var s=document.createElement('style');s.id='code-rs-boot-background';s.textContent='html,body{{background:'+c+'}}';(document.head||document.documentElement).appendChild(s);}}catch(e){{}}}})();"#
    )
}

fn background_file<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join(BACKGROUND_FILE))
}

/// Parses `#rgb`, `#rrggbb` or `#rrggbbaa`.
pub fn parse_color(s: &str) -> Option<Color> {
    let hex = s.trim().strip_prefix('#')?;
    let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    match hex.len() {
        3 => {
            let d = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok().map(|v| v * 17);
            Some(Color(d(0)?, d(1)?, d(2)?, 255))
        }
        6 => Some(Color(byte(0)?, byte(2)?, byte(4)?, 255)),
        8 => Some(Color(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
        _ => None,
    }
}
