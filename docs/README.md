# code-rs documentation

Design notes and guides for the code-rs fork. Upstream VS Code documentation lives in the
[VS Code wiki](https://github.com/microsoft/vscode/wiki).

The notes are in [`rust-rewrite/`](rust-rewrite). Source comments link to these paths, so files
keep their names.

## Getting started

| Document | What it covers |
|---|---|
| [TAURI_SETUP.md](rust-rewrite/TAURI_SETUP.md) | Building and running code-rs: requirements, dev mode, release packaging |

## Architecture

| Document | What it covers |
|---|---|
| [ELECTRON_INVENTORY.md](rust-rewrite/ELECTRON_INVENTORY.md) | What Electron/Node did in VS Code and what replaces it |
| [WIRING.md](rust-rewrite/WIRING.md) | How the Tauri bridges plug into the web workbench |
| [EXTHOST.md](rust-rewrite/EXTHOST.md) | The Node extension host, spawned and relayed by Rust |
| [PRETEXT.md](rust-rewrite/PRETEXT.md) | Pretext line wrapping: design and benchmark |

## AI features

| Document | What it covers |
|---|---|
| [MCP_DESIGN.md](rust-rewrite/MCP_DESIGN.md) | MCP server, approvals, command guardian, AI providers (Spanish) |
| [MUSE_PROTOCOL.md](rust-rewrite/MUSE_PROTOCOL.md) | Muse gadget protocol (Noise XX link, BLE pairing) |

## Elsewhere in the repository

| Path | What it is |
|---|---|
| [`resources/code-rs/`](../resources/code-rs) | Logo sources (SVG, 1024 px PNG) and how the app icons are generated |
| [`packaging/`](../packaging) | Distribution files: Arch Linux PKGBUILD, Claude Code skill |
| [`src-tauri/`](../src-tauri) | The Rust shell and its services (`src/services/`) |
| [`scripts/code-rs/`](../scripts/code-rs) | Release staging (`stage-app.mjs`) |
| [`.github/workflows/release.yml`](../.github/workflows/release.yml) | Release builds for every platform |
