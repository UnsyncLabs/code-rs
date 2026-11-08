# Wiring the Tauri bridges into the web workbench

This document describes how the four Tauri TypeScript bridges are wired into the
VS Code **web** workbench so that, when the workbench runs inside the Tauri 2
(Rust) shell (`code-rs`), it uses the native Rust services — and otherwise falls
back to normal web behaviour.

It complements:

- [`TAURI_SETUP.md`](./TAURI_SETUP.md) — how to build/run the shell.
- [`EXTHOST.md`](./EXTHOST.md) — the extension-host transport contract.
- [`ELECTRON_INVENTORY.md`](./ELECTRON_INVENTORY.md) — the full port inventory.

---

## 1. The four bridges (not edited here)

These files are owned by another agent; this change only *imports and uses*
their exported symbols:

| Bridge | Exports used |
|---|---|
| `src/vs/platform/files/browser/tauriFileSystemProvider.ts` | `TauriFileSystemProvider` |
| `src/vs/platform/terminal/browser/tauriPty.ts` | `createTauriPty` |
| `src/vs/workbench/services/search/browser/tauriSearchProvider.ts` | `TauriSearchProvider` |
| `src/vs/workbench/services/extensions/browser/tauriExtensionHost.ts` | `TauriExtensionHost` |

All four detect Tauri through the injected global `window.__TAURI_INTERNALS__`
(the ext host additionally needs `window.__TAURI__`, i.e.
`app.withGlobalTauri: true`).

---

## 2. Tauri detection

New file: **`src/vs/platform/tauri/common/tauri.ts`**

- `isTauri()` — `true` when `window.__TAURI_INTERNALS__` is present. This is the
  single gate used across all wiring points.
- `hasTauriGlobalApi()` — `true` when `window.__TAURI__` is present (needed by
  the extension-host bridge).
- `TAURI_FOLDER_QUERY_PARAM = 'tauriFolder'` — the URL query key the shell uses
  to pass the folder to open.

It lives under `platform/.../common` so both the core bootstrap
(`browser/web.main.ts`) and workbench contributions can import it without
crossing layering boundaries.

---

## 3. What was wired, and where

### 3.1 File system provider (`file` scheme)

**Where:** `src/vs/workbench/browser/web.main.ts`,
`BrowserMain.registerIndexedDBFileSystemProviders()`.

The web workbench normally registers the browser `HTMLFileSystemProvider` for
the `file` scheme *only if* `WebFileSystemAccess.supported(window)`. That branch
now becomes:

```ts
if (isTauri()) {
    fileService.registerProvider(Schemas.file, this._register(new TauriFileSystemProvider()));
} else if (WebFileSystemAccess.supported(mainWindow)) {
    fileService.registerProvider(Schemas.file, new HTMLFileSystemProvider(...));
}
```

This is the one place the `file` provider is installed, so registering the
Tauri provider here (rather than in the contribution) avoids a
double-registration throw from `FileService.registerProvider`.

### 3.2 Terminal backend

**Where:**
- New `src/vs/workbench/contrib/tauri/browser/tauriTerminalBackend.ts` —
  `TauriTerminalBackend implements ITerminalBackend`.
- Registered by the contribution (below) into the
  `TerminalExtensions.Backend` registry for the local authority
  (`remoteAuthority === undefined`), then announced via
  `ITerminalInstanceService.didRegisterBackend`.

`createProcess(...)` maps onto `createTauriPty(shell, args, cwd, env, cols,
rows)`. The backend is **non-persistent** (the Rust pty host is in-process):
attach/revive/list/layout/latency are safe no-ops, and it does not extend
`BaseTerminalBackend` (which assumes an `IPtyHostController`). The default shell
comes from `$SHELL` / `%COMSPEC%` with a platform fallback.

### 3.3 Search provider (`file` scheme, file + text)

**Where:** the contribution registers `TauriSearchProvider` with the
`ISearchService`:

```ts
searchService.registerSearchResultProvider(Schemas.file, SearchProviderType.file, provider);
searchService.registerSearchResultProvider(Schemas.file, SearchProviderType.text, provider);
```

### 3.4 Extension host (local process via Node sidecar)

**Where:** `src/vs/workbench/services/extensions/browser/extensionService.ts`
(the only subsystem where the host *factory* is composed, so wiring is kept
local to it):

1. `ExtensionService` now passes `hasLocalProcess: isTauri()` to
   `AbstractExtensionService` (was hard-coded `false`), so local-process
   extensions are resolved when in Tauri.
2. `BrowserExtensionHostFactory.createExtensionHost(...)`, `LocalProcess` branch:
   when `isTauri()` it returns
   `instantiationService.createInstance(TauriExtensionHost, runningLocation,
   startup, dataProvider)` instead of `null`. The data provider reuses the
   existing `_createLocalExtensionHostDataProvider` (its
   `{ extensions }` shape matches `ITauriExtensionHostDataProvider`).
3. `BrowserExtensionHostKindPicker.pickExtensionHostKind(...)`: when `isTauri()`
   and the extension is installed locally with a `ui`/`workspace` kind (and the
   preference is not `Remote`), it picks `ExtensionHostKind.LocalProcess`.

### 3.5 Open a local folder on startup

**Where:** `src/vs/code/browser/workbench/workbench.ts`,
`WorkspaceProvider.create(...)`.

When no `?folder=`/`?workspace=`/`?ew=` is present and no server-provided
workspace exists, and `isTauri()` is true, the `?tauriFolder=<abs path>` query
value is turned into a `file://` folder URI and used as the default workspace:

```ts
const tauriFolder = query.get(TAURI_FOLDER_QUERY_PARAM);
if (tauriFolder) {
    workspace = { folderUri: URI.file(tauriFolder) };
}
```

### 3.6 Contribution + entry point

- New `src/vs/workbench/contrib/tauri/browser/tauri.contribution.ts` registers a
  `WorkbenchPhase.BlockStartup` contribution that — guarded by `isTauri()` —
  wires the terminal backend and search provider. It self-no-ops outside Tauri.
- It is imported from `src/vs/workbench/workbench.web.main.ts` alongside the
  other web contributions.

---

## 4. Files changed / added

Added:

- `src/vs/platform/tauri/common/tauri.ts`
- `src/vs/workbench/contrib/tauri/browser/tauri.contribution.ts`
- `src/vs/workbench/contrib/tauri/browser/tauriTerminalBackend.ts`

Edited (minimally):

- `src/vs/workbench/browser/web.main.ts` — `file` provider branch + imports.
- `src/vs/workbench/services/extensions/browser/extensionService.ts` —
  `hasLocalProcess`, `LocalProcess` factory branch, kind picker, imports.
- `src/vs/code/browser/workbench/workbench.ts` — default workspace from
  `tauriFolder` + imports.
- `src/vs/workbench/workbench.web.main.ts` — import the contribution.

---

## 5. Remaining gaps / TODO

- **Verification pending.** `node_modules` was not installed in the authoring
  environment, so a full `tsc`/gulp build was not run here. The changes were
  type-reviewed against the referenced interfaces; run `pnpm run watch-web`
  (see `TAURI_SETUP.md`) to confirm.
- **`withGlobalTauri`.** The extension-host bridge reads `window.__TAURI__`,
  which requires `app.withGlobalTauri: true` in `src-tauri/tauri.conf.json`
  (owned by the shell agent). Until set, `start()` will throw a clear error. The
  fs/pty/search bridges work off `__TAURI_INTERNALS__` and are unaffected.
- **Ext host init data is partial.** `TauriExtensionHost._createInitData()`
  forwards only the extensions snapshot (see `EXTHOST.md`). Full
  `IExtensionHostInitData` (product/version, workspace, telemetry, logs, env
  URIs…) must be assembled like
  `localProcessExtensionHost._createExtHostInitData` once the browser
  environment/product/profile services are threaded through. The host boots but
  may mis-report environment.
- **Terminal parity.** No profile detection, env inheritance reporting, WSL path
  translation, flow control/backpressure, or process-tree listing. The default
  shell is resolved from env with a platform fallback only.
- **No terminal persistence / reconnection.** A renderer reload starts fresh
  (the Rust pty host and the Node ext host both accept a single connection).
- **Search scope.** Only the `file` scheme is wired; AI text search is not.
- **Native service stubs.** The Rust `src-tauri/src/services/*` handlers are
  still stubs (per `TAURI_SETUP.md`); wiring is complete on the TS side but
  end-to-end behaviour depends on those being implemented.
