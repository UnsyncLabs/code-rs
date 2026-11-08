//! Filesystem service.
//!
//! Replaces the Node `fs` access the Electron main process gave the workbench.
//! Mirrors the semantics of `IFileSystemProvider` from
//! `src/vs/platform/files/common/files.ts`:
//!
//! * [`FileType`] matches the TS `FileType` enum bit values
//!   (`Unknown=0`, `File=1`, `Directory=2`, `SymbolicLink=64`).
//! * Errors are serialized as [`ProviderError`] carrying a
//!   [`FileSystemProviderErrorCode`] string whose values match the TS
//!   `FileSystemProviderErrorCode` enum (`EntryNotFound`, `EntryExists`, ...).
//!   The browser-side `tauriFileSystemProvider.ts` re-throws them through
//!   `createFileSystemProviderError` so workbench code sees native VS Code
//!   errors.
//!
//! Binary safety: file contents cross the IPC boundary as base64 strings so
//! arbitrary bytes survive the JSON transport (the workbench deals in
//! `Uint8Array`).

use std::io;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Mirrors `FileSystemProviderErrorCode` in
/// `src/vs/platform/files/common/files.ts`. The `serde` rename values are the
/// exact string codes the TS layer expects.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum FileSystemProviderErrorCode {
    #[serde(rename = "EntryExists")]
    FileExists,
    #[serde(rename = "EntryNotFound")]
    FileNotFound,
    #[serde(rename = "EntryNotADirectory")]
    FileNotADirectory,
    #[serde(rename = "EntryIsADirectory")]
    FileIsADirectory,
    #[serde(rename = "EntryExceedsStorageQuota")]
    FileExceedsStorageQuota,
    #[serde(rename = "EntryTooLarge")]
    FileTooLarge,
    #[serde(rename = "EntryWriteLocked")]
    FileWriteLocked,
    #[serde(rename = "NoPermissions")]
    NoPermissions,
    #[serde(rename = "Unavailable")]
    Unavailable,
    #[serde(rename = "Unknown")]
    Unknown,
}

/// Serializable error returned to the frontend. `tauri::command` requires the
/// error type to implement `Serialize`; the TS side reads `.code` and `.message`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderError {
    pub code: FileSystemProviderErrorCode,
    pub message: String,
}

impl ProviderError {
    fn new(code: FileSystemProviderErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProviderError {}

/// Map a `std::io::Error` to the closest `FileSystemProviderErrorCode`,
/// mirroring how `diskFileSystemProvider.ts` interprets Node errno codes.
impl From<io::Error> for ProviderError {
    fn from(err: io::Error) -> Self {
        use io::ErrorKind::*;
        let code = match err.kind() {
            NotFound => FileSystemProviderErrorCode::FileNotFound,
            AlreadyExists => FileSystemProviderErrorCode::FileExists,
            PermissionDenied => FileSystemProviderErrorCode::NoPermissions,
            _ => {
                // Fall back to inspecting the raw OS error for codes that have
                // no stable `ErrorKind` on stable Rust (e.g. ENOTDIR, EISDIR).
                // NOTE: Unix errno and Windows system error codes share the
                // same `raw_os_error()` namespace per platform; the arms below
                // are deliberately non-overlapping so they compile on both.
                match err.raw_os_error() {
                    #[cfg(unix)]
                    Some(2) => FileSystemProviderErrorCode::FileNotFound, // ENOENT
                    #[cfg(unix)]
                    Some(1) | Some(13) => FileSystemProviderErrorCode::NoPermissions, // EPERM / EACCES
                    #[cfg(unix)]
                    Some(17) => FileSystemProviderErrorCode::FileExists, // EEXIST
                    #[cfg(unix)]
                    Some(20) => FileSystemProviderErrorCode::FileNotADirectory, // ENOTDIR
                    #[cfg(unix)]
                    Some(21) => FileSystemProviderErrorCode::FileIsADirectory, // EISDIR

                    #[cfg(windows)]
                    Some(2) | Some(3) => FileSystemProviderErrorCode::FileNotFound, // FILE/PATH_NOT_FOUND
                    #[cfg(windows)]
                    Some(5) => FileSystemProviderErrorCode::NoPermissions, // ACCESS_DENIED
                    #[cfg(windows)]
                    Some(80) | Some(183) => FileSystemProviderErrorCode::FileExists, // FILE_EXISTS / ALREADY_EXISTS
                    #[cfg(windows)]
                    Some(267) => FileSystemProviderErrorCode::FileNotADirectory, // DIRECTORY

                    _ => FileSystemProviderErrorCode::Unknown,
                }
            }
        };
        ProviderError::new(code, err.to_string())
    }
}

type Result<T> = std::result::Result<T, ProviderError>;

// ---------------------------------------------------------------------------
// Data types mirroring the TS layer
// ---------------------------------------------------------------------------

/// Mirrors the TS `FileType` enum bit values exactly so the frontend can use
/// the numeric value directly.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[repr(u32)]
pub enum FileType {
    Unknown = 0,
    File = 1,
    Directory = 2,
    SymbolicLink = 64,
}

/// Mirrors the TS `IStat` interface (times in millis since the Unix epoch).
#[derive(Debug, Serialize, Deserialize)]
pub struct FileStat {
    /// Numeric `FileType`. When the entry is a symlink the bit is OR-ed with
    /// the resolved target type, matching the VS Code convention that callers
    /// can still test for `File`/`Directory`.
    #[serde(rename = "type")]
    pub file_type: u32,
    pub mtime: u64,
    pub ctime: u64,
    pub size: u64,
    /// `FilePermission.Readonly` (1) when the target is read-only, else omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<u32>,
}

/// One `readdir` entry: `[name, FileType]` as the TS provider returns
/// `[string, FileType][]`.
#[derive(Debug, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub file_type: u32,
}

/// Mirrors `IFileWriteOptions` (the subset the disk provider honours).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteOptions {
    /// Create the file if it does not exist; error with `FileNotFound` otherwise.
    #[serde(default)]
    pub create: bool,
    /// Allow overwriting an existing file; error with `FileExists` otherwise.
    #[serde(default)]
    pub overwrite: bool,
    /// Write atomically (temp file in the same dir + rename over the target).
    #[serde(default)]
    pub atomic: bool,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            create: true,
            overwrite: true,
            atomic: false,
        }
    }
}

/// Mirrors `IFileDeleteOptions`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteOptions {
    #[serde(default)]
    pub recursive: bool,
    #[serde(default)]
    pub use_trash: bool,
}

/// Mirrors `IFileOverwriteOptions` used by rename/copy.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverwriteOptions {
    #[serde(default)]
    pub overwrite: bool,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn file_type_from_metadata(meta: &std::fs::Metadata, symlink: &std::fs::Metadata) -> u32 {
    let mut ty = if meta.is_dir() {
        FileType::Directory as u32
    } else if meta.is_file() {
        FileType::File as u32
    } else {
        FileType::Unknown as u32
    };
    if symlink.file_type().is_symlink() {
        ty |= FileType::SymbolicLink as u32;
    }
    ty
}

fn millis(time: std::io::Result<std::time::SystemTime>) -> u64 {
    time.ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Produce an error with an explicit code and message (for cases that aren't a
/// raw `io::Error`, e.g. honouring `create`/`overwrite` flags).
fn err(code: FileSystemProviderErrorCode, message: impl Into<String>) -> ProviderError {
    ProviderError::new(code, message)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// `stat(resource)` -> `IStat`.
#[tauri::command]
pub async fn stat(path: String) -> Result<FileStat> {
    let p = PathBuf::from(&path);
    // `symlink_metadata` first so we can detect symlinks, then resolve the
    // target metadata for type/size (matching VS Code semantics).
    let link_meta = std::fs::symlink_metadata(&p)?;
    let meta = std::fs::metadata(&p).unwrap_or_else(|_| link_meta.clone());

    let permissions = if meta.permissions().readonly() {
        Some(1 /* FilePermission.Readonly */)
    } else {
        None
    };

    Ok(FileStat {
        file_type: file_type_from_metadata(&meta, &link_meta),
        mtime: millis(meta.modified()),
        ctime: millis(meta.created()),
        size: meta.len(),
        permissions,
    })
}

/// `readFile(resource)` -> base64 of the raw bytes.
#[tauri::command]
pub async fn read_file(path: String) -> Result<String> {
    let bytes = std::fs::read(&path).map_err(ProviderError::from)?;
    Ok(BASE64.encode(bytes))
}

/// `writeFile(resource, content, opts)` where `contents` is base64.
#[tauri::command]
pub async fn write_file(
    path: String,
    contents: String,
    options: Option<WriteOptions>,
) -> Result<()> {
    let opts = options.unwrap_or_default();
    let p = PathBuf::from(&path);
    let bytes = BASE64
        .decode(contents.as_bytes())
        .map_err(|e| err(FileSystemProviderErrorCode::Unknown, format!("invalid base64: {e}")))?;

    let exists = p.exists();
    if exists {
        // Guard: writing over a directory is invalid.
        if p.is_dir() {
            return Err(err(
                FileSystemProviderErrorCode::FileIsADirectory,
                format!("File is a directory: {path}"),
            ));
        }
        if !opts.overwrite {
            return Err(err(
                FileSystemProviderErrorCode::FileExists,
                format!("File already exists: {path}"),
            ));
        }
    } else if !opts.create {
        return Err(err(
            FileSystemProviderErrorCode::FileNotFound,
            format!("File does not exist: {path}"),
        ));
    }

    if opts.atomic {
        write_atomic(&p, &bytes)
    } else {
        std::fs::write(&p, &bytes).map_err(ProviderError::from)
    }
}

/// Atomic write: write to a sibling temp file then rename over the target so a
/// reader never observes a partially written file.
fn write_atomic(target: &Path, bytes: &[u8]) -> Result<()> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    // Unique-ish postfix; the same-folder requirement keeps the rename atomic
    // (same filesystem). Uses process id + nanos to avoid collisions.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(".{file_name}.{}.{nanos}.vsctmp", std::process::id()));

    std::fs::write(&tmp, bytes).map_err(ProviderError::from)?;
    match std::fs::rename(&tmp, target) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Best-effort cleanup of the temp file before surfacing the error.
            let _ = std::fs::remove_file(&tmp);
            Err(ProviderError::from(e))
        }
    }
}

/// `readdir(resource)` -> `[string, FileType][]` (emitted as objects).
#[tauri::command]
pub async fn read_dir(path: String) -> Result<Vec<DirEntry>> {
    let mut entries = Vec::new();
    let read = std::fs::read_dir(&path).map_err(ProviderError::from)?;
    for entry in read {
        let entry = entry.map_err(ProviderError::from)?;
        let name = entry.file_name().to_string_lossy().into_owned();

        // Use the cheap DirEntry::file_type (no extra stat) when possible, but
        // resolve symlink targets to mirror VS Code's behaviour.
        let link_ft = entry.file_type().map_err(ProviderError::from)?;
        let file_type = if link_ft.is_symlink() {
            // Resolve the target; unknown if it dangles.
            match std::fs::metadata(entry.path()) {
                Ok(meta) => {
                    let base = if meta.is_dir() {
                        FileType::Directory as u32
                    } else if meta.is_file() {
                        FileType::File as u32
                    } else {
                        FileType::Unknown as u32
                    };
                    base | FileType::SymbolicLink as u32
                }
                Err(_) => FileType::Unknown as u32 | FileType::SymbolicLink as u32,
            }
        } else if link_ft.is_dir() {
            FileType::Directory as u32
        } else if link_ft.is_file() {
            FileType::File as u32
        } else {
            FileType::Unknown as u32
        };

        entries.push(DirEntry { name, file_type });
    }
    Ok(entries)
}

/// `mkdir(resource)` — creates a single directory (parents assumed to exist,
/// matching `IFileSystemProvider.mkdir`).
#[tauri::command]
pub async fn mkdir(path: String) -> Result<()> {
    match std::fs::create_dir(&path) {
        Ok(()) => Ok(()),
        // create_dir reports AlreadyExists; surface as FileExists.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(err(
            FileSystemProviderErrorCode::FileExists,
            format!("Directory already exists: {path}"),
        )),
        Err(e) => Err(ProviderError::from(e)),
    }
}

/// `delete(resource, opts)` — honours `recursive` and `useTrash`.
#[tauri::command]
pub async fn delete(path: String, options: Option<DeleteOptions>) -> Result<()> {
    let opts = options.unwrap_or_default();
    let p = PathBuf::from(&path);

    let link_meta = std::fs::symlink_metadata(&p).map_err(ProviderError::from)?;

    if opts.use_trash {
        // The `trash` crate handles files and directories (recursively) and
        // never permanently deletes, so `recursive` is implicit there.
        return trash::delete(&p).map_err(map_trash_error);
    }

    if link_meta.file_type().is_symlink() || link_meta.is_file() {
        std::fs::remove_file(&p).map_err(ProviderError::from)
    } else if link_meta.is_dir() {
        if opts.recursive {
            std::fs::remove_dir_all(&p).map_err(ProviderError::from)
        } else {
            // Non-recursive: errors if the directory is not empty (ENOTEMPTY).
            std::fs::remove_dir(&p).map_err(ProviderError::from)
        }
    } else {
        // Unknown entry kind: best-effort remove_file.
        std::fs::remove_file(&p).map_err(ProviderError::from)
    }
}

fn map_trash_error(e: trash::Error) -> ProviderError {
    use trash::Error as TE;
    let code = match &e {
        TE::CouldNotAccess { .. } => FileSystemProviderErrorCode::FileNotFound,
        TE::CanonicalizePath { .. } => FileSystemProviderErrorCode::FileNotFound,
        _ => FileSystemProviderErrorCode::Unknown,
    };
    ProviderError::new(code, format!("trash failed: {e:?}"))
}

/// `rename(from, to, opts)` — move/rename honouring `overwrite`.
#[tauri::command]
pub async fn rename(from: String, to: String, options: Option<OverwriteOptions>) -> Result<()> {
    let opts = options.unwrap_or_default();
    let src = PathBuf::from(&from);
    let dst = PathBuf::from(&to);

    if std::fs::symlink_metadata(&src).is_err() {
        return Err(err(
            FileSystemProviderErrorCode::FileNotFound,
            format!("Source does not exist: {from}"),
        ));
    }

    if dst.exists() {
        if !opts.overwrite {
            return Err(err(
                FileSystemProviderErrorCode::FileExists,
                format!("Target already exists: {to}"),
            ));
        }
        // Overwrite: remove the existing target first. `std::fs::rename` already
        // overwrites files on both Unix and Windows, but a pre-existing
        // directory target would fail, so clear it explicitly.
        remove_any(&dst)?;
    }

    std::fs::rename(&src, &dst).map_err(ProviderError::from)
}

fn remove_any(p: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(p).map_err(ProviderError::from)?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        std::fs::remove_dir_all(p).map_err(ProviderError::from)
    } else {
        std::fs::remove_file(p).map_err(ProviderError::from)
    }
}

/// `copy(from, to, opts)` — recursive copy honouring `overwrite`.
#[tauri::command]
pub async fn copy(from: String, to: String, options: Option<OverwriteOptions>) -> Result<()> {
    let opts = options.unwrap_or_default();
    let src = PathBuf::from(&from);
    let dst = PathBuf::from(&to);

    let src_meta = std::fs::symlink_metadata(&src).map_err(|_| {
        err(
            FileSystemProviderErrorCode::FileNotFound,
            format!("Source does not exist: {from}"),
        )
    })?;

    if dst.exists() {
        if !opts.overwrite {
            return Err(err(
                FileSystemProviderErrorCode::FileExists,
                format!("Target already exists: {to}"),
            ));
        }
        remove_any(&dst)?;
    }

    if src_meta.is_dir() {
        copy_dir_recursive(&src, &dst)
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(ProviderError::from)?;
        }
        std::fs::copy(&src, &dst).map(|_| ()).map_err(ProviderError::from)
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(ProviderError::from)?;
    for entry in std::fs::read_dir(src).map_err(ProviderError::from)? {
        let entry = entry.map_err(ProviderError::from)?;
        let child_src = entry.path();
        let child_dst = dst.join(entry.file_name());
        let ft = entry.file_type().map_err(ProviderError::from)?;
        if ft.is_dir() {
            copy_dir_recursive(&child_src, &child_dst)?;
        } else {
            // Symlinks are copied as their target contents (std::fs::copy follows
            // the link), which is acceptable for the workbench copy semantics.
            std::fs::copy(&child_src, &child_dst)
                .map(|_| ())
                .map_err(ProviderError::from)?;
        }
    }
    Ok(())
}

/// `realpath(resource)` — resolve symlinks / `.`/`..` to an absolute path.
#[tauri::command]
pub async fn realpath(path: String) -> Result<String> {
    let resolved = std::fs::canonicalize(&path).map_err(ProviderError::from)?;
    // Strip the Windows verbatim prefix (\\?\) that canonicalize adds, so the
    // path round-trips through VS Code's URI handling cleanly.
    let s = resolved.to_string_lossy().into_owned();
    Ok(strip_verbatim(s))
}

#[cfg(windows)]
fn strip_verbatim(s: String) -> String {
    if let Some(stripped) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{stripped}")
    } else if let Some(stripped) = s.strip_prefix(r"\\?\") {
        stripped.to_string()
    } else {
        s
    }
}

#[cfg(not(windows))]
fn strip_verbatim(s: String) -> String {
    s
}
