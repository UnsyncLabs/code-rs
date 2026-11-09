//! Installs gallery extensions (VSIX) to disk so they can run in the Node
//! extension host.
//!
//! The web workbench only knows "web extensions" (referenced by URL and run in a
//! web worker). Most extensions (Python, linters, formatters, ...) need Node, so
//! inside code-rs the VSIX is downloaded here, its `extension/` folder is
//! extracted into `<app data>/extensions/<publisher>.<name>-<version>` and the
//! workbench registers that folder as a `file://` extension location.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use tauri::{AppHandle, Manager, Runtime};

/// Maximum VSIX size accepted (extensions like Python are ~30 MB).
const MAX_VSIX_BYTES: u64 = 512 * 1024 * 1024;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallVsixRequest {
    /// VSIX download URL from the gallery (`assets.download.uri`).
    pub url: String,
    /// `publisher.name` (used for the folder name).
    pub id: String,
    pub version: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallVsixResult {
    /// Absolute path of the extracted extension folder.
    pub path: String,
}

/// Downloads a VSIX from the gallery and extracts it. Returns the folder.
#[tauri::command]
pub async fn extension_install_vsix<R: Runtime>(
    app: AppHandle<R>,
    request: InstallVsixRequest,
) -> Result<InstallVsixResult, String> {
    let url = request.url.clone();
    if !(url.starts_with("https://")) {
        return Err(format!("refusing non-https extension download: {url}"));
    }
    let folder_name = sanitize(&format!("{}-{}", request.id.to_lowercase(), request.version));
    let root = extensions_dir(&app)?;
    let target = root.join(&folder_name);

    tauri::async_runtime::spawn_blocking(move || -> Result<InstallVsixResult, String> {
        let bytes = download(&url)?;
        let staging = root.join(format!(".{folder_name}.installing"));
        let _ = std::fs::remove_dir_all(&staging);
        extract_extension_folder(&bytes, &staging)?;
        let _ = std::fs::remove_dir_all(&target);
        std::fs::rename(&staging, &target).map_err(|e| format!("rename: {e}"))?;
        Ok(InstallVsixResult { path: target.to_string_lossy().into_owned() })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Removes an installed extension folder (only inside the extensions dir).
#[tauri::command]
pub async fn extension_uninstall_folder<R: Runtime>(app: AppHandle<R>, path: String) -> Result<(), String> {
    let root = extensions_dir(&app)?;
    let path = PathBuf::from(path);
    let canonical_root = std::fs::canonicalize(&root).map_err(|e| e.to_string())?;
    let canonical = std::fs::canonicalize(&path).map_err(|e| e.to_string())?;
    if !canonical.starts_with(&canonical_root) || canonical == canonical_root {
        return Err("refusing to delete outside the extensions folder".into());
    }
    std::fs::remove_dir_all(canonical).map_err(|e| e.to_string())
}

fn extensions_dir<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("extensions");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    let response = ureq::get(url)
        .header("User-Agent", "code-rs")
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let mut bytes = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(MAX_VSIX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("download failed: {e}"))?;
    if bytes.len() as u64 > MAX_VSIX_BYTES {
        return Err("extension package is too large".into());
    }
    Ok(bytes)
}

/// Extracts the `extension/` folder of a VSIX (a zip) into `dest`.
fn extract_extension_folder(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| format!("invalid VSIX: {e}"))?;
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let mut found_manifest = false;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        // `enclosed_name` rejects absolute paths and `..` (zip-slip).
        let Some(name) = entry.enclosed_name() else { continue };
        let Ok(rel) = name.strip_prefix("extension") else { continue };
        if rel.as_os_str().is_empty() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            continue;
        }
        if rel == Path::new("package.json") {
            found_manifest = true;
        }
        let out = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut file = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut file).map_err(|e| e.to_string())?;
    }
    if !found_manifest {
        let _ = std::fs::remove_dir_all(dest);
        return Err("invalid VSIX: no extension/package.json".into());
    }
    Ok(())
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect()
}

/// `win32-x64`, `linux-arm64`, `darwin-arm64`, ... (VS Code target platform).
#[tauri::command]
pub fn host_target_platform() -> String {
    let os = match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "arm" => "armhf",
        other => other,
    };
    format!("{os}-{arch}")
}
