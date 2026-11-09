//! Self-update support.
//!
//! Installs that Tauri can update in place (Windows NSIS/MSI, macOS .app,
//! Linux AppImage) use `tauri-plugin-updater`, verified against the public key
//! in tauri.conf.json, with `latest.json` published by the release workflow.
//! Installs owned by a system package manager (.deb, .rpm, pacman) or local
//! builds only report the latest version and open the releases page instead.

use tauri::{AppHandle, Runtime};
use tauri_plugin_updater::UpdaterExt;

pub const RELEASES_URL: &str = "https://github.com/UnsyncLabs/code-rs/releases/latest";
const LATEST_RELEASE_API: &str = "https://api.github.com/repos/UnsyncLabs/code-rs/releases/latest";

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdaterInfo {
    pub current_version: String,
    /// "installer" (in-place updates), "package-manager" (deb/rpm/pacman) or "dev".
    pub install_kind: &'static str,
    pub can_self_update: bool,
    pub releases_url: &'static str,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableUpdate {
    pub version: String,
    pub notes: Option<String>,
}

fn install_kind() -> &'static str {
    if cfg!(debug_assertions) {
        "dev"
    } else if cfg!(target_os = "linux") && std::env::var_os("APPIMAGE").is_none() {
        "package-manager"
    } else {
        "installer"
    }
}

#[tauri::command]
pub fn updater_info<R: Runtime>(app: AppHandle<R>) -> UpdaterInfo {
    let kind = install_kind();
    UpdaterInfo {
        current_version: app.package_info().version.to_string(),
        install_kind: kind,
        can_self_update: kind == "installer",
        releases_url: RELEASES_URL,
    }
}

/// Returns the newer version if there is one.
#[tauri::command]
pub async fn updater_check<R: Runtime>(app: AppHandle<R>) -> Result<Option<AvailableUpdate>, String> {
    if install_kind() == "installer" {
        let update = app
            .updater()
            .map_err(|e| e.to_string())?
            .check()
            .await
            .map_err(|e| e.to_string())?;
        return Ok(update.map(|u| AvailableUpdate { version: u.version, notes: u.body }));
    }
    // Package-manager / dev builds: just compare with the latest GitHub release.
    let current = app.package_info().version.to_string();
    let latest = tauri::async_runtime::spawn_blocking(latest_release_tag)
        .await
        .map_err(|e| e.to_string())??;
    let latest = latest.trim_start_matches('v').to_string();
    Ok(is_newer(&latest, &current).then_some(AvailableUpdate { version: latest, notes: None }))
}

/// Downloads, verifies and installs the update, then restarts the app.
#[tauri::command]
pub async fn updater_install<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    if install_kind() != "installer" {
        return Err("this installation is managed by a package manager; download the new version from the releases page".into());
    }
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("code-rs is up to date")?;
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    app.restart();
}

/// Opens the releases page in the default browser.
#[tauri::command]
pub fn updater_open_releases() -> Result<(), String> {
    tauri_plugin_opener::open_url(RELEASES_URL, None::<&str>).map_err(|e| e.to_string())
}

fn latest_release_tag() -> Result<String, String> {
    let text = ureq::get(LATEST_RELEASE_API)
        .header("User-Agent", "code-rs")
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| e.to_string())?
        .into_body()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let body: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    body.get("tag_name")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| "no tag_name in latest release".into())
}

/// Compares dotted numeric versions (`0.10.0` > `0.9.3`); pre-release suffixes are ignored.
fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split(['-', '+']).next().unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0)).collect()
    };
    parse(candidate) > parse(current)
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("0.10.0", "0.9.3"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0-beta", "0.1.0"));
    }
}
