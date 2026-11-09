//! Native services exposed to the VS Code web workbench.
//!
//! Each submodule maps to a capability that Electron used to provide through
//! Node.js integration. They are currently stubs: the `tauri::command`
//! signatures are in place and compile, but the bodies are `TODO`s.

pub mod acp;
pub mod app_protocol;
pub mod assistant;
pub mod guardian;
pub mod extension_host;
pub mod extension_audit;
pub mod extension_install;
pub mod fs;
pub mod mcp;
pub mod pty;
pub mod search;
pub mod updater;
pub mod watcher;
pub mod webview_server;
pub mod window;
