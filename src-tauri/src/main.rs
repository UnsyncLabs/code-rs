// Prevents an additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `code-rs --mcp`: stdio bridge for MCP clients (Claude Code, Kiro, ...)
    // to the running code-rs instance; no window is opened.
    if std::env::args().any(|a| a == "--mcp") {
        std::process::exit(code_rs_lib::mcp_stdio_bridge());
    }
    code_rs_lib::run();
}
