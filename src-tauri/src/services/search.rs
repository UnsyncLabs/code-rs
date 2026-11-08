//! Text and file search service.
//!
//! Replaces VS Code's ripgrep-backed search provider. Instead of shelling out
//! to a bundled `rg` binary, this embeds ripgrep's own library crates:
//!
//! - [`ignore`]        — parallel, `.gitignore`-aware directory traversal.
//! - [`globset`]       — include / exclude glob matching.
//! - [`grep_regex`]    — the regex matcher (`RegexMatcher`).
//! - [`grep_searcher`] — the line/multiline searcher + `Sink` streaming API.
//! - [`grep_matcher`]  — the `Matcher` trait (column extraction).
//!
//! Results are streamed to the workbench as Tauri events rather than buffered
//! and returned in bulk, mirroring how the Node search host pushes
//! `ISerializedFileMatch` progress items:
//!
//! - `search://result/{id}` — one [`FileMatchEvent`] per file that matched.
//! - `search://done/{id}`    — a single [`SearchDoneEvent`] with `limit_hit`.
//!
//! The payload shapes mirror `IFileMatch` / `ITextSearchMatch` /
//! `ISearchRange` from `src/vs/workbench/services/search/common/search.ts` so
//! the TS side can revive them with minimal translation.
//!
//! Each query carries a client-chosen `id`; [`search_cancel`] flips an
//! `AtomicBool` shared with the running walk so in-flight searches stop
//! promptly.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use globset::{Glob, GlobSet, GlobSetBuilder};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch};
use ignore::{WalkBuilder, WalkState};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Runtime, State};

// ---------------------------------------------------------------------------
// Wire shapes (mirror src/vs/workbench/services/search/common/search.ts)
// ---------------------------------------------------------------------------

/// Mirrors `ISearchRange`. 1-based line numbers, 0-based columns — matching the
/// convention used by the ripgrep node host before the workbench converts to
/// editor coordinates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRange {
    #[serde(rename = "startLineNumber")]
    pub start_line_number: u64,
    #[serde(rename = "startColumn")]
    pub start_column: u64,
    #[serde(rename = "endLineNumber")]
    pub end_line_number: u64,
    #[serde(rename = "endColumn")]
    pub end_column: u64,
}

/// Mirrors `SearchRangeSetPairing { source, preview }`.
///
/// `source` is the range in the original file; `preview` is the range within
/// `preview_text` (which may be a trimmed / context-augmented slice).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRangeSetPairing {
    pub source: SearchRange,
    pub preview: SearchRange,
}

/// Mirrors `ITextSearchMatch`. `preview_text` serializes as `previewText`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextSearchMatch {
    #[serde(rename = "rangeLocations")]
    pub range_locations: Vec<SearchRangeSetPairing>,
    #[serde(rename = "previewText")]
    pub preview_text: String,
}

/// Mirrors `IFileMatch`. The workbench reconstitutes `resource` from `path`
/// (the Rust side only ever has a filesystem path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMatchEvent {
    /// The search id this result belongs to (also encoded in the event name).
    pub id: String,
    /// Absolute filesystem path of the matching file.
    pub path: String,
    /// Text matches within the file. Empty for file-name search results.
    pub results: Vec<TextSearchMatch>,
}

/// Payload for `search://done/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchDoneEvent {
    pub id: String,
    /// True if `max_results` was reached and results were truncated.
    #[serde(rename = "limitHit")]
    pub limit_hit: bool,
    /// Present only when the search failed; mirrors a serialized search error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Query inputs
// ---------------------------------------------------------------------------

/// Text (content) search query. Field names use camelCase on the wire to match
/// the workbench's `IPatternInfo` / `ITextQuery` naming.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextSearchQuery {
    /// Client-chosen id used for event routing and cancellation.
    pub id: String,
    pub pattern: String,
    /// Root folders to search. At least one is expected.
    pub folders: Vec<String>,
    #[serde(default, rename = "isRegExp")]
    pub is_regexp: bool,
    #[serde(default, rename = "isCaseSensitive")]
    pub is_case_sensitive: bool,
    #[serde(default, rename = "isWordMatch")]
    pub is_word_match: bool,
    /// Include globs (e.g. `**/*.ts`). Empty means "all files".
    #[serde(default)]
    pub includes: Vec<String>,
    /// Exclude globs (e.g. `**/node_modules/**`).
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default, rename = "maxResults")]
    pub max_results: Option<usize>,
    /// Number of context lines before/after each match for the preview.
    #[serde(default, rename = "previewContextLines")]
    pub preview_context_lines: Option<usize>,
    /// When false, `.gitignore` / ignore files are not honored.
    #[serde(default = "default_true", rename = "useIgnoreFiles")]
    pub use_ignore_files: bool,
    #[serde(default, rename = "followSymlinks")]
    pub follow_symlinks: bool,
}

/// Fuzzy file-name search query (quick open style).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSearchQuery {
    pub id: String,
    /// Fuzzy file-name pattern. Empty lists all (ignore-honoring) files.
    #[serde(default)]
    pub pattern: String,
    pub folders: Vec<String>,
    #[serde(default)]
    pub includes: Vec<String>,
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default, rename = "maxResults")]
    pub max_results: Option<usize>,
    #[serde(default = "default_true", rename = "useIgnoreFiles")]
    pub use_ignore_files: bool,
    #[serde(default, rename = "followSymlinks")]
    pub follow_symlinks: bool,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Cancellation registry (Tauri managed state)
// ---------------------------------------------------------------------------

/// Shared cancellation flags keyed by search id. Registered with Tauri via
/// `app.manage(SearchState::default())` in `lib.rs` setup.
#[derive(Default)]
pub struct SearchState {
    cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl SearchState {
    /// Registers (or replaces) a cancellation flag for `id` and returns it.
    fn register(&self, id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        let mut map = self.cancels.lock().expect("search cancel map poisoned");
        map.insert(id.to_string(), flag.clone());
        flag
    }

    /// Signals cancellation for `id`, if a search with that id is registered.
    fn cancel(&self, id: &str) {
        if let Some(flag) = self
            .cancels
            .lock()
            .expect("search cancel map poisoned")
            .get(id)
        {
            flag.store(true, Ordering::SeqCst);
        }
    }

    /// Removes the flag for `id` once the search has finished.
    fn finish(&self, id: &str) {
        self.cancels
            .lock()
            .expect("search cancel map poisoned")
            .remove(id);
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Searches file contents for `query.pattern`, streaming `search://result/{id}`
/// events and finishing with `search://done/{id}`.
///
/// Returns immediately with `Ok(())`; the walk runs on a blocking thread pool
/// so it does not stall the async runtime. All results — including errors — are
/// reported through events, so the frontend only needs to subscribe.
#[tauri::command]
pub async fn search_text<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, SearchState>,
    query: TextSearchQuery,
) -> Result<(), String> {
    let cancel = state.register(&query.id);
    let app_for_task = app.clone();
    let id = query.id.clone();

    // ignore::Walk already uses its own thread pool; run the driver on a
    // blocking thread so the (synchronous) traversal never blocks the async
    // executor. We spawn and detach: completion is signalled via events.
    tauri::async_runtime::spawn_blocking(move || {
        let result = run_text_search(&app_for_task, &query, cancel);
        // emit_done clears the cancel-flag registry entry via managed state.
        emit_done(&app_for_task, &id, result);
    });

    // The detached task owns its own Arc clone of the cancel flag, so dropping
    // the `state` borrow here is fine. Registry cleanup happens in emit_done.
    Ok(())
}

/// Lists files whose names fuzzily match `query.pattern`, honoring
/// `.gitignore`, include/exclude globs, and `max_results`. Streams the same
/// `search://result/{id}` / `search://done/{id}` events (with empty `results`).
#[tauri::command]
pub async fn search_files<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, SearchState>,
    query: FileSearchQuery,
) -> Result<(), String> {
    let cancel = state.register(&query.id);
    let app_for_task = app.clone();
    let id = query.id.clone();

    tauri::async_runtime::spawn_blocking(move || {
        let result = run_file_search(&app_for_task, &query, cancel);
        emit_done(&app_for_task, &id, result);
    });

    Ok(())
}

/// Cancels an in-flight search by id. Safe to call for unknown/finished ids.
#[tauri::command]
pub fn search_cancel(state: State<'_, SearchState>, id: String) -> Result<(), String> {
    state.cancel(&id);
    state.finish(&id);
    Ok(())
}

// ---------------------------------------------------------------------------
// Event helpers
// ---------------------------------------------------------------------------

fn result_event_name(id: &str) -> String {
    format!("search://result/{id}")
}

fn done_event_name(id: &str) -> String {
    format!("search://done/{id}")
}

fn emit_file_match<R: Runtime>(app: &AppHandle<R>, id: &str, path: &Path, results: Vec<TextSearchMatch>) {
    let payload = FileMatchEvent {
        id: id.to_string(),
        path: path.to_string_lossy().into_owned(),
        results,
    };
    // Emit failures (e.g. window gone) are non-fatal for the search itself.
    let _ = app.emit(&result_event_name(id), payload);
}

/// Emits the completion event and clears the cancel-flag registry entry.
fn emit_done<R: Runtime>(app: &AppHandle<R>, id: &str, result: Result<bool, String>) {
    let (limit_hit, error) = match result {
        Ok(limit_hit) => (limit_hit, None),
        Err(e) => (false, Some(e)),
    };
    let payload = SearchDoneEvent {
        id: id.to_string(),
        limit_hit,
        error,
    };
    let _ = app.emit(&done_event_name(id), payload);

    // Best-effort cleanup of the cancel flag now that the search is done.
    if let Some(state) = app.try_state::<SearchState>() {
        state.finish(id);
    }
}

// ---------------------------------------------------------------------------
// Glob / matcher construction
// ---------------------------------------------------------------------------

fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let glob = Glob::new(pat).map_err(|e| format!("invalid glob '{pat}': {e}"))?;
        builder.add(glob);
    }
    builder
        .build()
        .map(Some)
        .map_err(|e| format!("failed to build glob set: {e}"))
}

/// Builds a `RegexMatcher` from the query, applying case sensitivity, whole-word
/// matching, and literal (`fixed_strings`) matching when `is_regexp` is false.
fn build_matcher(
    pattern: &str,
    is_regexp: bool,
    is_case_sensitive: bool,
    is_word_match: bool,
) -> Result<RegexMatcher, String> {
    RegexMatcherBuilder::new()
        .case_insensitive(!is_case_sensitive)
        .word(is_word_match)
        // When not a regex, match the pattern verbatim (meta chars literal).
        .fixed_strings(!is_regexp)
        // Keep multiline off: we stream line-oriented results like rg default.
        .multi_line(false)
        .build(pattern)
        .map_err(|e| format!("invalid search pattern: {e}"))
}

// ---------------------------------------------------------------------------
// Text search implementation
// ---------------------------------------------------------------------------

/// Returns `Ok(limit_hit)` on success.
fn run_text_search<R: Runtime>(
    app: &AppHandle<R>,
    query: &TextSearchQuery,
    cancel: Arc<AtomicBool>,
) -> Result<bool, String> {
    if query.folders.is_empty() {
        return Err("search_text: no folders provided".into());
    }

    let matcher = build_matcher(
        &query.pattern,
        query.is_regexp,
        query.is_case_sensitive,
        query.is_word_match,
    )?;

    let includes = build_globset(&query.includes)?;
    let excludes = build_globset(&query.excludes)?;
    let context = query.preview_context_lines.unwrap_or(0) as usize;

    let walker = build_walker(
        &query.folders,
        query.use_ignore_files,
        query.follow_symlinks,
    );

    // Shared, thread-safe accounting across the parallel walk.
    let match_count = Arc::new(AtomicUsize::new(0));
    let limit_hit = Arc::new(AtomicBool::new(false));
    let max_results = query.max_results;

    walker.build_parallel().run(|| {
        // Per-thread clones of the shared handles.
        let app = app.clone();
        let matcher = matcher.clone();
        let includes = includes.clone();
        let excludes = excludes.clone();
        let cancel = cancel.clone();
        let match_count = match_count.clone();
        let limit_hit = limit_hit.clone();
        let id = query.id.clone();

        // Each thread owns its Searcher (it is not Sync).
        let mut searcher: Searcher = SearcherBuilder::new()
            .line_number(true)
            .before_context(context)
            .after_context(context)
            .build();

        Box::new(move |entry| {
            if cancel.load(Ordering::SeqCst) {
                return WalkState::Quit;
            }
            if limit_hit.load(Ordering::SeqCst) {
                return WalkState::Quit;
            }

            let entry = match entry {
                Ok(e) => e,
                Err(_) => return WalkState::Continue,
            };

            // Only search regular files.
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                return WalkState::Continue;
            }

            let path = entry.path();
            if !path_passes_filters(path, includes.as_ref(), excludes.as_ref()) {
                return WalkState::Continue;
            }

            let mut sink = TextSink {
                matcher: &matcher,
                matches: Vec::new(),
                preview_context: context,
                cancel: &cancel,
                limit_hit: &limit_hit,
                match_count: &match_count,
                max_results,
            };

            // Errors here (binary files, IO) are per-file and non-fatal.
            let _ = searcher.search_path(&matcher, path, &mut sink);

            if !sink.matches.is_empty() {
                emit_file_match(&app, &id, path, std::mem::take(&mut sink.matches));
            }

            if limit_hit.load(Ordering::SeqCst) || cancel.load(Ordering::SeqCst) {
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
    });

    Ok(limit_hit.load(Ordering::SeqCst))
}

/// A `grep_searcher::Sink` that collects per-line matches into
/// [`TextSearchMatch`]es, tracking column ranges via the matcher and honoring a
/// global result cap.
struct TextSink<'a> {
    matcher: &'a RegexMatcher,
    matches: Vec<TextSearchMatch>,
    preview_context: usize,
    cancel: &'a AtomicBool,
    limit_hit: &'a AtomicBool,
    match_count: &'a AtomicUsize,
    max_results: Option<usize>,
}

impl<'a> TextSink<'a> {
    /// Returns false if the global result limit has now been reached (so the
    /// searcher should stop).
    fn bump_and_check_limit(&self) -> bool {
        if let Some(max) = self.max_results {
            // fetch_add returns the previous value.
            let prev = self.match_count.fetch_add(1, Ordering::SeqCst);
            if prev + 1 >= max {
                self.limit_hit.store(true, Ordering::SeqCst);
                return false;
            }
        } else {
            self.match_count.fetch_add(1, Ordering::SeqCst);
        }
        true
    }
}

impl<'a> Sink for TextSink<'a> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, std::io::Error> {
        if self.cancel.load(Ordering::SeqCst) || self.limit_hit.load(Ordering::SeqCst) {
            return Ok(false);
        }

        let line_number = mat.line_number().unwrap_or(0);
        let bytes = mat.bytes();
        let line_text = String::from_utf8_lossy(bytes);
        // Strip a single trailing line terminator for the preview text.
        let preview_text = line_text
            .trim_end_matches('\n')
            .trim_end_matches('\r')
            .to_string();

        // Find every column span of the pattern within this line.
        let mut range_locations = Vec::new();
        let mut keep_searching = true;
        let find_result = self.matcher.find_iter(bytes, |m| {
            let (start_col, end_col) = byte_range_to_columns(bytes, m.start(), m.end());
            range_locations.push(SearchRangeSetPairing {
                source: SearchRange {
                    start_line_number: line_number,
                    start_column: start_col,
                    end_line_number: line_number,
                    end_column: end_col,
                },
                // When no context lines are requested, preview is the single
                // matched line so preview line index is 0.
                preview: SearchRange {
                    start_line_number: 0,
                    start_column: start_col,
                    end_line_number: 0,
                    end_column: end_col,
                },
            });
            if !self.bump_and_check_limit() {
                keep_searching = false;
                return false;
            }
            true
        });
        // find_iter can fail on the matcher; ignore and keep whatever we found.
        let _ = find_result;

        if !range_locations.is_empty() {
            self.matches.push(TextSearchMatch {
                range_locations,
                preview_text,
            });
        }

        let _ = self.preview_context;
        Ok(keep_searching)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, std::io::Error> {
        // Context lines are emitted alongside matches for richer previews.
        // VS Code's ITextSearchContext is a separate result kind; here we fold
        // the context into the preview by appending a zero-range entry so the
        // TS side can render surrounding lines. Only 'before'/'after' kinds are
        // relevant.
        match ctx.kind() {
            SinkContextKind::Before | SinkContextKind::After => {
                if let Some(last) = self.matches.last_mut() {
                    let line = String::from_utf8_lossy(ctx.bytes());
                    let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                    // Append context as additional preview text separated by a
                    // newline so the match keeps its surrounding lines.
                    last.preview_text.push('\n');
                    last.preview_text.push_str(trimmed);
                }
            }
            SinkContextKind::Other => {}
        }
        Ok(true)
    }
}

/// Converts a byte offset range within `line` to 0-based UTF-16-agnostic column
/// indices measured in Unicode scalar values (chars). VS Code columns are
/// 0-based at the ripgrep-host boundary; the workbench converts to its own
/// 1-based editor columns later.
fn byte_range_to_columns(line: &[u8], start: usize, end: usize) -> (u64, u64) {
    let text = String::from_utf8_lossy(line);
    // Map byte offsets to char counts.
    let start_col = text
        .char_indices()
        .take_while(|(i, _)| *i < start)
        .count() as u64;
    let end_col = text
        .char_indices()
        .take_while(|(i, _)| *i < end)
        .count() as u64;
    (start_col, end_col)
}

// ---------------------------------------------------------------------------
// File (name) search implementation
// ---------------------------------------------------------------------------

fn run_file_search<R: Runtime>(
    app: &AppHandle<R>,
    query: &FileSearchQuery,
    cancel: Arc<AtomicBool>,
) -> Result<bool, String> {
    if query.folders.is_empty() {
        return Err("search_files: no folders provided".into());
    }

    let includes = build_globset(&query.includes)?;
    let excludes = build_globset(&query.excludes)?;
    let pattern_lower = query.pattern.to_lowercase();
    let has_pattern = !pattern_lower.is_empty();

    let walker = build_walker(
        &query.folders,
        query.use_ignore_files,
        query.follow_symlinks,
    );

    let match_count = Arc::new(AtomicUsize::new(0));
    let limit_hit = Arc::new(AtomicBool::new(false));
    let max_results = query.max_results;

    walker.build_parallel().run(|| {
        let app = app.clone();
        let includes = includes.clone();
        let excludes = excludes.clone();
        let cancel = cancel.clone();
        let match_count = match_count.clone();
        let limit_hit = limit_hit.clone();
        let pattern_lower = pattern_lower.clone();
        let id = query.id.clone();

        Box::new(move |entry| {
            if cancel.load(Ordering::SeqCst) || limit_hit.load(Ordering::SeqCst) {
                return WalkState::Quit;
            }

            let entry = match entry {
                Ok(e) => e,
                Err(_) => return WalkState::Continue,
            };

            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                return WalkState::Continue;
            }

            let path = entry.path();
            if !path_passes_filters(path, includes.as_ref(), excludes.as_ref()) {
                return WalkState::Continue;
            }

            // Fuzzy match against the file name (fallback to full path).
            if has_pattern {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                let haystack = if fuzzy_contains(&name, &pattern_lower) {
                    true
                } else {
                    fuzzy_contains(&path.to_string_lossy().to_lowercase(), &pattern_lower)
                };
                if !haystack {
                    return WalkState::Continue;
                }
            }

            // File-name results carry no text matches.
            emit_file_match(&app, &id, path, Vec::new());

            if let Some(max) = max_results {
                let prev = match_count.fetch_add(1, Ordering::SeqCst);
                if prev + 1 >= max {
                    limit_hit.store(true, Ordering::SeqCst);
                    return WalkState::Quit;
                }
            } else {
                match_count.fetch_add(1, Ordering::SeqCst);
            }

            WalkState::Continue
        })
    });

    Ok(limit_hit.load(Ordering::SeqCst))
}

/// Order-preserving subsequence match, mirroring VS Code's `fuzzyContains`:
/// every character of `pattern` must appear in `target` in order.
fn fuzzy_contains(target: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return true;
    }
    let mut pat_chars = pattern.chars().peekable();
    for ch in target.chars() {
        if let Some(&p) = pat_chars.peek() {
            if ch == p {
                pat_chars.next();
            }
        } else {
            break;
        }
    }
    pat_chars.peek().is_none()
}

// ---------------------------------------------------------------------------
// Shared walk helpers
// ---------------------------------------------------------------------------

/// Builds an `ignore::WalkBuilder` seeded with every folder and configured for
/// `.gitignore` honoring and symlink following.
fn build_walker(folders: &[String], use_ignore_files: bool, follow_symlinks: bool) -> WalkBuilder {
    let mut it = folders.iter();
    // WalkBuilder::new requires a first path; subsequent paths are `add`ed.
    let first = it.next().expect("folders checked non-empty by caller");
    let mut builder = WalkBuilder::new(first);
    for f in it {
        builder.add(f);
    }

    builder
        .follow_links(follow_symlinks)
        .hidden(false) // let .gitignore decide; don't auto-skip dotfiles
        .git_ignore(use_ignore_files)
        .git_global(use_ignore_files)
        .git_exclude(use_ignore_files)
        .ignore(use_ignore_files)
        .parents(use_ignore_files);
    builder
}

/// Applies include/exclude glob filters. Excludes win over includes.
fn path_passes_filters(path: &Path, includes: Option<&GlobSet>, excludes: Option<&GlobSet>) -> bool {
    if let Some(ex) = excludes {
        if ex.is_match(path) {
            return false;
        }
    }
    if let Some(inc) = includes {
        if !inc.is_match(path) {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Tests (compiled/run on Windows where Rust is available)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_contains_matches_subsequence() {
        assert!(fuzzy_contains("searchservice.ts", "sst"));
        assert!(fuzzy_contains("searchservice.ts", "search"));
        assert!(!fuzzy_contains("abc", "cab"));
        assert!(fuzzy_contains("anything", ""));
    }

    #[test]
    fn columns_are_char_based() {
        // "héllo" — 'é' is 2 bytes; the match for "llo" starts at byte 3.
        let line = "héllo".as_bytes();
        // bytes: h(0) é(1,2) l(3) l(4) o(5)
        let (s, e) = byte_range_to_columns(line, 3, 6);
        assert_eq!(s, 2); // char index of first 'l'
        assert_eq!(e, 5); // char index past 'o'
    }

    #[test]
    fn literal_pattern_is_escaped() {
        // A regex-special pattern used literally should still build.
        let m = build_matcher("a.b(c)", false, true, false);
        assert!(m.is_ok());
    }
}
