# Electron / Node-main dependency inventory

Counts are approximate (grep-based, `src/vs`, tests included unless noted). Difficulty: L/M/H.

## Headline numbers
- `src/vs/code/electron-main`: 2 files (`main.ts`, `app.ts` ~1965 lines), the main-process composition root.
- 60 `electron-main` dirs (about 55 non-test), mostly under `src/vs/platform/*`; plus `src/vs/base/parts/{contextmenu,ipc}/electron-main`.
- `electron-browser` (renderer): 163 dirs / ~424 .ts files. No `electron-sandbox` dirs remain. `electron-utility`: 4 dirs / 14 files.
- Files with `from 'electron'`: 56. Files mentioning `ipcRenderer`/`ipcMain`: 45.
- Renderer talks to main through `src/vs/base/parts/sandbox/electron-browser` (preload globals `vscode.ipcRenderer`, `webFrame`, `process`, `context`) and `vscode:*` channel names (~100+ distinct, e.g. `vscode:message`, `vscode:reloadWindow`, `vscode:openFiles`, `vscode:createPtyHostMessageChannel`, `vscode:createAgentHostMessageChannel`, `vscode:browserView:*`, `vscode:toggleDevTools`, `vscode:runAction`).
- Biggest files: `app.ts` (1965 lines), `windowsMainService.ts` (1882), `nativeHostMainService.ts` (1741).

## Architectural note (key decision)
Most heavy services (fs provider, watcher, pty host, search, extension host, log) already live in `*/node` and run in utility/child processes behind the VS Code IPC channel protocol. Pragmatic path: keep the TS `common`/`browser` workbench, replace Electron main + preload with Tauri commands/events, and either (a) keep a Node sidecar for the extension host (unavoidable: extensions are JS) or (b) reimplement each node service in Rust behind the same `IChannel` names so the workbench is unchanged.

## Per-service table

| Service | Key paths | Count | Rust / Tauri replacement | Diff |
|---|---|---|---|---|
| Filesystem | `src/vs/platform/files/node/diskFileSystemProvider*.ts`, `files/electron-main`, `files/common` (keep) | ~6 node files | `tokio::fs`; implement `IFileSystemProvider` channel in Rust (read/write/stat/readdir/atomic write; trash via `trash` crate). `tauri-plugin-fs` is too coarse | M |
| File watcher | `files/node/watcher/*`, `@parcel/watcher` (2 files) | ~15 | `notify` + `notify-debouncer-full`; needs recursive, exclude globs, correlation ids | M |
| Terminal / pty | `platform/terminal/node/*` (ptyHost, ptyService, terminalProcess, windowsShellHelper), `terminal/electron-main/electronPtyHostStarter.ts`, `node-pty` (8 files), `windows-process-tree` (5) | ~20 | `portable-pty` (ConPTY on Windows), `sysinfo` for process tree; replace `MessagePort` with Tauri `ipc::Channel` streaming | H |
| Search | `workbench/services/search/node/*` (ripgrep wrappers), `@vscode/ripgrep` (47 file mentions) | ~10 core | ship `rg` as sidecar via `tauri-plugin-shell` (lowest risk) or `grep`/`ignore` crates | L (sidecar) / M |
| Windows / menus / dialogs | `platform/windows/electron-main` (6), `window/electron-main`, `auxiliaryWindow/electron-main` (3), `menubar/electron-main`, `dialogs/electron-main`, `native/electron-main/nativeHostMainService.ts`, `base/parts/contextmenu/electron-main`, `theme/electron-main`, `workspaces/electron-main` (3) | ~35 | `WebviewWindow`, `tauri::menu` (muda), `tauri-plugin-dialog`, window-state plugin; custom titlebar/zoom/fullscreen. Multi-window + auxiliary windows + state restore is the hard part | H |
| Lifecycle | `platform/lifecycle/electron-main`, `launch/electron-main`, `code/electron-main/{main,app}.ts` | ~5 | Tauri `RunEvent`, `tauri-plugin-single-instance`, deep-link plugin; reimplement shutdown veto (`onBeforeUnload`/`onWillUnload`) | M |
| Update | `platform/update/electron-main` (9: win32/darwin/linux/snap) | 9 | `tauri-plugin-updater` (signed artifacts, different manifest) | M |
| Storage | `platform/storage/electron-main` (3), `@vscode/sqlite3` (27 file mentions), `backup/electron-main`, `userDataProfile/electron-main` | ~12 | `rusqlite` (bundled); keep `state.vscdb` schema; expose storage channel | M |
| Secrets / encryption | `platform/encryption/electron-main` (Electron `safeStorage`), `keytar` (2 files, legacy) | ~4 | `keyring` crate (Credential Manager / Keychain / libsecret) | L |
| Keyboard layout | `platform/keyboardLayout/electron-main`, `native-keymap` (4) | ~5 | No mature crate: Win32 `GetKeyboardLayout`/`ToUnicodeEx`, macOS TIS, Linux `xkbcommon`; or fall back to DOM `KeyboardEvent.code` | H |
| Clipboard | `nativeHostMainService` (text, image, macOS find pasteboard) | few | `tauri-plugin-clipboard-manager` (find pasteboard needs `objc2`) | L |
| Extension host launch | `workbench/services/extensions/electron-browser/localProcessExtensionHost.ts`, `platform/utilityProcess/electron-main` (2), `platform/extensions/electron-main`, `workbench/api/node/*`, `@vscode/proxy-agent` (13), `kerberos` (7) | ~25 | Node sidecar via `tauri-plugin-shell` + stdio/socket protocol replacing `MessagePort`. Biggest blocker to a pure-Rust stack; keep Node initially | H |
| Shared process | `platform/sharedProcess/electron-main/sharedProcess.ts`, `workbench/services/sharedProcess/electron-browser`, `code/electron-browser/sharedProcess`, `code/node/sharedProcess` | ~6 | Sidecar or in-process Rust tasks (ext management, telemetry, sync, pty host, diagnostics) | H |
| Utility process / MessagePort | `utilityProcess/electron-main`, `base/parts/ipc/electron-main` (3), `base/parts/ipc/electron-browser`, `utilityProcessWorkerMainService.ts` | ~10 | Tauri `ipc::Channel` + events; custom `IChannel` bridge over invoke/events | M |
| Protocol / webview | `platform/protocol/electron-main` (4: `vscode-file://`, `vscode-webview://`), `webview/electron-main` (2), `webContentExtractor` (4), `browserView/electron-main` (15) | ~25 | `register_uri_scheme_protocol`; webview origin isolation is hard; browserView (embedded Chromium views) has no stable Tauri 2 equivalent | H |
| Logging | `platform/log/electron-main`, `@vscode/spdlog` (6) | ~8 | `tracing` + `tracing-appender` | L |
| Policy / env / misc | `@vscode/policy-watcher` (4), `native-is-elevated` (3), `@vscode/native-watchdog` (3), `@vscode/deviceid` (4), `@vscode/windows-registry`, `windows-mutex`, `windows-foreground-love`, `environment/`, `diagnostics/`, `gpu/`, `profiling/`, `externalTerminal/`, `globalKeybindings/`, `meteredConnection/`, `url/`, `debug/`, `sandbox/`, `onboarding/`, `agentHost/` (all `electron-main`) | ~30 | `windows` crate registry, `is_elevated`, `machine-uid`, `sysinfo`; Tauri global-shortcut/deep-link/shell plugins | L-M |

## Native modules (package.json)
`node-pty`, `@parcel/watcher`, `@vscode/ripgrep-universal`, `@vscode/spdlog`, `@vscode/sqlite3`, `native-keymap`, `@vscode/policy-watcher`, `@vscode/windows-process-tree`, `@vscode/windows-registry`, `@vscode/windows-mutex`, `windows-foreground-love`, `native-is-elevated`, `@vscode/native-watchdog`, `kerberos`, `@vscode/deviceid`, `@vscode/proxy-agent` (JS, node-only), `keytar` / Electron `safeStorage`. They only keep working if a Node sidecar is kept; each has a Rust equivalent above.

## Suggested port order
1. Shell: window, `vscode-file` protocol, preload shim replacing `vscode.ipcRenderer`/`process`/`context` globals.
2. Storage, logging, secrets, clipboard, dialogs (low risk).
3. fs provider + watcher, search (ripgrep sidecar).
4. Pty host.
5. Menus, multi-window, lifecycle, update.
6. Extension host + shared process (Node sidecar), keyboard layout, webviews/browserView last.
