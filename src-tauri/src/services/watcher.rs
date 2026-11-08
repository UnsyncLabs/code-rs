//! File-watching service.
//!
//! Replaces VS Code's Node-side file watcher (`@parcel/watcher` + chokidar).
//! Built on the `notify` crate with `notify-debouncer-full` so rapid bursts of
//! low-level OS events collapse into a single logical change, and rename
//! `From`/`To` pairs are stitched together.
//!
//! Each [`watch`] call returns an opaque `id`. Change events are pushed to the
//! frontend out-of-band as a Tauri event named [`FS_CHANGE_EVENT`]
//! (`"fs://change"`) whose payload matches VS Code's `IFileChange`:
//!
//! ```jsonc
//! { "resource": "/abs/path", "type": "added" | "updated" | "deleted", "cId": 7 }
//! ```
//!
//! `resource` is an absolute filesystem path (the TS provider converts it to a
//! `file://` URI); `type` is the string form of `FileChangeType`; `cId` is the
//! optional correlation id echoed from the watch request.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::notify::Watcher as _;
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, FileIdMap};
use notify::RecommendedWatcher;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Wry};

/// Name of the Tauri event carrying file changes to the workbench.
pub const FS_CHANGE_EVENT: &str = "fs://change";

/// How long to coalesce bursts of raw events before emitting.
const DEBOUNCE: Duration = Duration::from_millis(100);

/// Mirrors `IWatchOptions` (the subset we honour). `recursive` toggles watching
/// of children; `excludes` is a set of glob patterns / paths matched relative
/// to the watched folder; `correlationId` is echoed back on each event.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchOptions {
    #[serde(default)]
    pub recursive: bool,
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default)]
    pub correlation_id: Option<u64>,
}

/// Mirrors a single `IFileChange`. `type` is the string form of the TS
/// `FileChangeType` enum.
#[derive(Debug, Clone, Serialize)]
pub struct FileChange {
    pub resource: String,
    #[serde(rename = "type")]
    pub change_type: &'static str,
    #[serde(rename = "cId", skip_serializing_if = "Option::is_none")]
    pub c_id: Option<u64>,
}

const CHANGE_ADDED: &str = "added";
const CHANGE_UPDATED: &str = "updated";
const CHANGE_DELETED: &str = "deleted";

/// A single live watch: the debouncer guard (dropping it stops the watch) plus
/// the request parameters retained for debugging and future re-filtering
/// (e.g. an `unwatch`-by-resource pass or dynamic exclude updates).
struct ActiveWatch {
    _debouncer: Debouncer<RecommendedWatcher, FileIdMap>,
    #[allow(dead_code)]
    root: PathBuf,
    #[allow(dead_code)]
    excludes: Vec<String>,
    #[allow(dead_code)]
    correlation_id: Option<u64>,
}

/// Registry of active watchers, stored as Tauri managed state.
#[derive(Default)]
pub struct WatcherState {
    next_id: AtomicU64,
    watches: Mutex<HashMap<u64, ActiveWatch>>,
}

impl WatcherState {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            watches: Mutex::new(HashMap::new()),
        }
    }
}

/// Very small glob matcher supporting the subset VS Code exclude patterns use:
/// `**` (any path segments), `*` (any run of non-separator chars), and literal
/// segments. Matching is done on the path relative to the watched root, with
/// `/`-normalised separators. Absolute/relative literal paths also match as a
/// prefix. This avoids pulling in a glob crate while covering common excludes
/// like `**/node_modules/**` and `**/.git/**`.
fn is_excluded(root: &Path, changed: &Path, excludes: &[String]) -> bool {
    if excludes.is_empty() {
        return false;
    }

    let rel = changed.strip_prefix(root).unwrap_or(changed);
    let rel_str = normalize_sep(&rel.to_string_lossy());
    let abs_str = normalize_sep(&changed.to_string_lossy());

    for pattern in excludes {
        let pat = normalize_sep(pattern);
        if glob_match(&pat, &rel_str) || glob_match(&pat, &abs_str) {
            return true;
        }
        // Treat a bare directory name (e.g. "node_modules") as "**/name/**".
        if !pat.contains('/') && !pat.contains('*') {
            if rel_str
                .split('/')
                .any(|segment| segment == pat)
            {
                return true;
            }
        }
    }
    false
}

fn normalize_sep(s: &str) -> String {
    s.replace('\\', "/")
}

/// Minimal glob matcher for `*` and `**`. `**` matches across `/`; `*` matches
/// within a single segment.
fn glob_match(pattern: &str, text: &str) -> bool {
    // Convert to a simple recursive matcher over byte slices of the two
    // `/`-normalised strings.
    fn helper(p: &[&str], t: &[&str]) -> bool {
        // p / t are split by '/'. Walk segment by segment.
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(&"**"), _) => {
                // `**` consumes zero or more segments.
                helper(&p[1..], t) || (!t.is_empty() && helper(p, &t[1..]))
            }
            (Some(seg), Some(tseg)) => {
                if segment_match(seg, tseg) {
                    helper(&p[1..], &t[1..])
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    let p: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let t: Vec<&str> = text.split('/').filter(|s| !s.is_empty()).collect();
    helper(&p, &t)
}

/// Match a single path segment against a pattern segment containing `*`.
fn segment_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == text;
    }
    // Split the pattern on `*` and ensure the literal parts appear in order.
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut idx = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            // Must match at the start.
            if !text[idx..].starts_with(part) {
                return false;
            }
            idx += part.len();
        } else if i == parts.len() - 1 {
            // Must match at the end.
            if !text[idx..].ends_with(part) {
                return false;
            }
            idx = text.len();
        } else {
            match text[idx..].find(part) {
                Some(found) => idx += found + part.len(),
                None => return false,
            }
        }
    }
    true
}

/// Translate a notify `EventKind` into the `IFileChange` string type. Returns
/// `None` for kinds we don't surface (e.g. pure access events).
fn classify(kind: &EventKind) -> Option<&'static str> {
    match kind {
        EventKind::Create(_) => Some(CHANGE_ADDED),
        EventKind::Remove(_) => Some(CHANGE_DELETED),
        // All modify kinds (data, metadata, and rename `Name` events) surface
        // as an update; the debouncer already stitched rename from/to pairs and
        // higher layers reconcile paths via subsequent add/remove events.
        EventKind::Modify(_) => Some(CHANGE_UPDATED),
        // Any/Other: be conservative and treat as an update so the workbench
        // re-reads rather than missing a change.
        EventKind::Any | EventKind::Other => Some(CHANGE_UPDATED),
        EventKind::Access(_) => None,
    }
}

/// `watch(path, options)` -> opaque watcher id.
#[tauri::command]
pub async fn watch(
    app: AppHandle<Wry>,
    state: tauri::State<'_, WatcherState>,
    path: String,
    options: Option<WatchOptions>,
) -> Result<u64, String> {
    let opts = options.unwrap_or_default();
    let root = PathBuf::from(&path);

    if !root.exists() {
        return Err(format!("Cannot watch non-existent path: {path}"));
    }

    let id = state.next_id.fetch_add(1, Ordering::SeqCst);

    // Clone the data the debounce callback needs; it runs on the debouncer's
    // own thread so everything moved in must be `Send + 'static`.
    let app_for_cb = app.clone();
    let root_for_cb = root.clone();
    let excludes_for_cb = opts.excludes.clone();
    let correlation_for_cb = opts.correlation_id;

    let mut debouncer = new_debouncer(
        DEBOUNCE,
        None,
        move |result: DebounceEventResult| match result {
            Ok(events) => {
                for debounced in events {
                    // `DebouncedEvent` wraps a `notify::Event` in its `.event`
                    // field (plus a `.time`); field access does not auto-deref.
                    let event = &debounced.event;
                    let Some(change_type) = classify(&event.kind) else {
                        continue;
                    };
                    for changed_path in &event.paths {
                        if is_excluded(&root_for_cb, changed_path, &excludes_for_cb) {
                            continue;
                        }
                        let change = FileChange {
                            resource: changed_path.to_string_lossy().into_owned(),
                            change_type,
                            c_id: correlation_for_cb,
                        };
                        // Emitting can fail only if the app is shutting down;
                        // ignore to keep the watcher thread alive.
                        let _ = app_for_cb.emit(FS_CHANGE_EVENT, change);
                    }
                }
            }
            Err(errors) => {
                for err in errors {
                    log::warn!("fs watcher error: {err:?}");
                }
            }
        },
    )
    .map_err(|e| format!("failed to create watcher: {e}"))?;

    let mode = if opts.recursive {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };

    debouncer
        .watcher()
        .watch(&root, mode)
        .map_err(|e| format!("failed to watch {path}: {e}"))?;
    // Keep the file-id cache in sync so rename stitching works on backends that
    // don't provide rename cookies (FSEvents / Windows).
    debouncer.cache().add_root(&root, mode);

    let active = ActiveWatch {
        _debouncer: debouncer,
        root,
        excludes: opts.excludes,
        correlation_id: opts.correlation_id,
    };

    state
        .watches
        .lock()
        .map_err(|_| "watcher state poisoned".to_string())?
        .insert(id, active);

    Ok(id)
}

/// `unwatch(id)` — stops and drops the watcher created by [`watch`].
#[tauri::command]
pub async fn unwatch(state: tauri::State<'_, WatcherState>, id: u64) -> Result<(), String> {
    let mut watches = state
        .watches
        .lock()
        .map_err(|_| "watcher state poisoned".to_string())?;
    // Dropping the `ActiveWatch` (and thus the debouncer guard) stops watching.
    watches.remove(&id);
    Ok(())
}

/// Helper so `lib.rs` can register the managed state in one call.
pub fn init(app: &AppHandle<Wry>) {
    app.manage(WatcherState::new());
}
