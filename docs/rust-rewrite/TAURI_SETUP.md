# code-rs — Tauri 2 shell setup

This document describes how to build and run **code-rs**: the VS Code TypeScript
web workbench hosted inside a [Tauri 2](https://v2.tauri.app/) (Rust) shell,
replacing Electron.

The Rust shell lives in [`src-tauri/`](../../src-tauri). It does **not**
re-implement the editor — it loads the existing compiled *web* workbench
(`src/vs/code/browser/workbench`) as its frontend and will expose native
capabilities (fs, file watching, pty, search, window, extension host) to it via
`tauri::command` handlers (currently stubs under `src-tauri/src/services/`).

---

## 1. Architecture at a glance

```
┌─────────────────────────────────────────────┐
│ Tauri 2 (Rust) — src-tauri/                   │
│   WebviewWindow                               │
│   ┌─────────────────────────────────────────┐ │
│   │ VS Code web workbench (compiled TS)     │ │
│   │   out/vs/code/browser/workbench/*.js    │ │
│   └─────────────────────────────────────────┘ │
│   native services (fs, watcher, pty, search,  │
│   window, extension_host) via invoke_handler  │
└─────────────────────────────────────────────┘
```

- **Frontend (dev):** the `@vscode/test-web` dev server started by
  `./scripts/code-web.sh`, served at `http://localhost:8080`. Tauri's
  `build.devUrl` points here.
- **Frontend (release):** the packaged workbench produced by
  `pnpm run gulp vscode-web-min`, written to `<repo>/../vscode-web`. Tauri's
  `build.frontendDist` points here (`../../vscode-web`, relative to
  `src-tauri/tauri.conf.json`).

---

## 2. Prerequisites

### All platforms
- **Rust** (stable, ≥ 1.77). Install via [rustup](https://rustup.rs/).
- **Tauri CLI v2:**
  ```sh
  cargo install tauri-cli --version '^2'
  # verify
  cargo tauri --version
  ```
- **Node.js** — the version pinned in [`.nvmrc`](../../.nvmrc)
  (**24.22.1** at time of writing). With nvm:
  ```sh
  nvm install   # reads .nvmrc
  nvm use
  ```
- **pnpm** — version from `package.json#packageManager` (`pnpm@12.16.0`):
  ```sh
  corepack enable
  corepack prepare pnpm@12.16.0 --activate
  ```

### Windows
- **Microsoft C++ Build Tools** (MSVC) — install the
  "Desktop development with C++" workload from the Visual Studio Build Tools,
  which provides the MSVC toolchain and Windows SDK required to link Rust.
- **WebView2 runtime** — preinstalled on Windows 10/11; otherwise install the
  Evergreen runtime from Microsoft.

### macOS
- **Xcode Command Line Tools:** `xcode-select --install`.

### Linux
- WebKitGTK and friends, e.g. on Debian/Ubuntu:
  ```sh
  sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
    libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
  ```

### Icons
Tauri's bundler needs icon files referenced in `tauri.conf.json`. They are not
committed — see [`src-tauri/icons/README.md`](../../src-tauri/icons/README.md).
Generate them with `cargo tauri icon path/to/icon.png`. A plain `cargo build` /
`cargo check` does not need icons; only bundling does.

---

## 3. Install JS dependencies

From the repo root:

```sh
pnpm install
```

---

## 4. Build the TypeScript web workbench

Pick **one** of the two workflows.

### Option A — Dev watch build (recommended for development)

Compiles the client/extensions into `out/` and keeps rebuilding on change:

```sh
# terminal 1 — from repo root
pnpm run watch          # or: pnpm run watch-web
```

Wait until the initial compile finishes (watchers print "Finished compilation").
This populates `out/vs/code/browser/workbench/workbench.js` etc.

Then start the web server that serves `out/` + sources:

```sh
# terminal 2 — from repo root
./scripts/code-web.sh                 # Linux/macOS
# or on Windows:
.\scripts\code-web.bat
```

This launches `@vscode/test-web` on `http://localhost:8080` (override with
`--port`). Tauri's `devUrl` already targets that URL.

### Option B — Production bundle

Produces a self-contained, minified web workbench:

```sh
# from repo root
pnpm run gulp vscode-web-min
```

Output is written to **`<repo>/../vscode-web`** (a *sibling* directory of this
repository — this is how the gulp `packageTask` is defined). Tauri's
`frontendDist` (`../../vscode-web`) resolves to exactly that folder.

> Non-minified variant: `pnpm run gulp vscode-web` (also outputs to
> `../vscode-web`).

---

## 5. Run code-rs

### Dev (uses `devUrl`, Option A above must be running)

```sh
# from repo root
cargo tauri dev
```

`cargo tauri dev` reads `src-tauri/tauri.conf.json`, opens a WebviewWindow, and
loads `http://localhost:8080`. `beforeDevCommand` is intentionally empty — start
the watch + web server yourself (Option A) because that pipeline is long-running
and owned by the VS Code build, not Tauri.

### Release (uses `frontendDist`, Option B above must have run)

```sh
# from repo root
cargo tauri build
```

This bundles the Rust shell together with the packaged `vscode-web` frontend.

### Plain Rust checks (no frontend needed)

```sh
cd src-tauri
cargo check      # type-check the Rust shell + service stubs
cargo build      # compile (does not require icons)
```

---

## 6. Current status / unresolved

- `src-tauri/src/services/*` are **stubs**: command signatures compile but every
  body returns a `"... not implemented"` error. Implementation tracked
  separately.
- Icons are placeholders (see `src-tauri/icons/README.md`).
- The native IPC bridge wiring the workbench to these commands (and the Node
  extension host launch protocol) is not implemented yet.
- CSP in `tauri.conf.json` is deliberately permissive to let the workbench boot;
  tighten before shipping.
- `src/vs/platform/telemetry` and `product.json` are owned by another agent and
  were not touched.
