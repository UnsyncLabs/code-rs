//! Text and file search service.
//!
//! Replaces VS Code's ripgrep-backed search provider. TODO: shell out to a
//! bundled `rg` binary (via tauri-plugin-shell) or embed the `grep`/`ignore`
//! crates, and stream results to the frontend.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct TextSearchQuery {
    pub pattern: String,
    pub folder: String,
    pub is_regex: bool,
    pub case_sensitive: bool,
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
    pub max_results: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TextSearchMatch {
    pub path: String,
    pub line: u64,
    pub column: u64,
    pub preview: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileSearchQuery {
    pub pattern: String,
    pub folder: String,
    pub excludes: Vec<String>,
    pub max_results: Option<u32>,
}

/// Searches file contents for `query.pattern`.
#[tauri::command]
pub async fn search_text(query: TextSearchQuery) -> Result<Vec<TextSearchMatch>, String> {
    let _ = query;
    Err("search::search_text not implemented".into())
}

/// Searches file names/paths matching `query.pattern` (quick open).
#[tauri::command]
pub async fn search_files(query: FileSearchQuery) -> Result<Vec<String>, String> {
    let _ = query;
    Err("search::search_files not implemented".into())
}
