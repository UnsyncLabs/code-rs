# Extension host on Tauri (code-rs)

How the VS Code **Node extension host** is launched and connected when the
Electron shell is replaced by the Tauri 2 (Rust) shell.

Owned files:

- `src-tauri/src/services/extension_host.rs` — Rust bridge (spawn + byte relay).
- `src/vs/workbench/services/extensions/browser/tauriExtensionHost.ts` —
  `IExtensionHost` implementation driving a `PersistentProtocol` over the relay.
- `src-tauri/src/lib.rs` — registers the commands + shared state (append-only).
- `src-tauri/Cargo.toml` — adds `tokio`, `interprocess`, `base64` (append-only).

---

## 1. Background: how Electron did it

Two files define the Electron behaviour:

- `src/vs/platform/extensions/electron-main/extensionHostStarter.ts` launches a
  **utility process** running `bootstrap-fork.js` with
  `entryPoint = vs/workbench/api/node/extensionHostProcess`.
- `src/vs/workbench/services/extensions/electron-browser/localProcessExtensionHost.ts`
  sets `VSCODE_ESM_ENTRYPOINT`, establishes a transport, and performs the
  `Ready` / init-data / `Initialized` handshake.

The extension host entry (`extensionHostProcess.ts::_createExtHostProtocol`)
supports three transports, chosen from the environment
(`extensionHostEnv.ts`):

| env key | transport | how the host connects |
|---|---|---|
| `VSCODE_WILL_SEND_MESSAGE_PORT` | Electron `MessagePort` | received over node IPC |
| `VSCODE_EXTHOST_WILL_SEND_SOCKET` | node `net.Socket` | received over node IPC |
| `VSCODE_EXTHOST_IPC_HOOK` | named pipe / unix domain socket | `net.createConnection(pipeName)` |

Electron used the **MessagePort** transport. In every transport the host wraps
its endpoint in a VS Code wire protocol:

- MessagePort → a thin `IMessagePassingProtocol`.
- Socket / IPC hook → a **`PersistentProtocol`** (13-byte header framing, acks,
  keep-alive, reconnection) and immediately calls `sendResume()`.

## 2. Why the IPC-hook transport on Tauri

Tauri has no way to transfer an Electron `MessagePort` (or a raw node socket
handle) to a child process. The only transport that needs **nothing but a path
string** is `VSCODE_EXTHOST_IPC_HOOK`: the host does
`net.createConnection(pipeName)` and talks `PersistentProtocol` over it.

So the Rust shell:

1. creates a **named pipe** (Windows) or **unix domain socket** (macOS/Linux),
2. passes its path to Node as `VSCODE_EXTHOST_IPC_HOOK`,
3. accepts the single inbound connection,
4. **relays raw bytes** between that socket and the webview.

The framing is owned by the two `PersistentProtocol` endpoints — one inside the
Node host, one inside the webview (`tauriExtensionHost.ts`). The bridge never
parses protocol bytes.

```
 webview                          Rust (src-tauri)                      Node child
 ───────                          ────────────────                      ──────────
 PersistentProtocol
   │ send(VSBuffer)                                                 extensionHostProcess
   │  → exthost_write(id, b64) ──►  mpsc ──► pipe.write ───────────►  net.Socket
   │                                                                   │ PersistentProtocol
   │ onMessage ◄── onData ◄── exthost://data/{id} ◄── pipe.read ◄──────┘  .sendResume()
```

## 3. Transport & message contract

Commands (webview → Rust):

| command | args | returns | meaning |
|---|---|---|---|
| `start_extension_host` | `{ options: ExtensionHostOptions }` | `u64` host id | spawn node + bind pipe + start relay |
| `exthost_write` | `{ id, bytes }` (`bytes` = base64) | `()` | forward bytes to the host socket |
| `stop_extension_host` | `{ id }` | `()` | graceful kill (closes socket, kills child) |

`ExtensionHostOptions` (camelCase over the wire):

```
nodePath?:      string   // else VSCODE_NODE, bundled runtime, or PATH
bootstrapPath?: string   // else <resourceDir>/out/bootstrap-fork.js
args:           string[] // appended after bootstrap-fork.js (e.g. --skipWorkspaceStorageLock)
execArgv:       string[] // node flags (e.g. --inspect-port=0)
env:            { [k]: string } // merged on top of process env
```

Events (Rust → webview):

| event | payload | meaning |
|---|---|---|
| `exthost://started/{id}` | `{ id, pid }` | child spawned |
| `exthost://data/{id}` | base64 `string` | raw bytes from the host socket |
| `exthost://exit/{id}` | `{ id, code, signal }` | child exited |

Environment set on the child by the Rust side:

- `VSCODE_ESM_ENTRYPOINT = vs/workbench/api/node/extensionHostProcess`
- `VSCODE_EXTHOST_IPC_HOOK = <pipe/socket path>`
- `VSCODE_HANDLES_UNCAUGHT_ERRORS = true`

Base64 uses the **standard** alphabet with padding on both ends
(`base64::STANDARD` in Rust, `encodeBase64`/`decodeBase64` defaults in TS).

## 4. Node discovery

`resolve_node_path` tries, in order:

1. `options.nodePath`,
2. `VSCODE_NODE` env var,
3. `<resourceDir>/node[.exe]` (bundled runtime convention),
4. `node`/`node.exe` on `PATH`.

`bootstrap-fork.js` is resolved from `options.bootstrapPath`, then
`<resourceDir>/out/bootstrap-fork.js`, then `<cwd>/out/bootstrap-fork.js`.

## 5. Lifecycle

- The listener is bound **before** `node` is spawned so the host's
  `connect()` cannot race the accept.
- A single supervisor task per host owns the `Child`, accepts exactly one
  connection, runs the read/write relay, awaits process exit, removes its
  handle from shared state, cleans up the unix socket file, and emits
  `exthost://exit/{id}`.
- `stop_extension_host` sends a `Kill` down the control channel; `disconnect()`
  in TS first sends a `Terminate` protocol message so extensions can deactivate.
- On Windows the child is spawned with `CREATE_NEW_PROCESS_GROUP`, mirroring
  Electron's `detached: isWindows` behaviour.

## 6. TS side (`tauriExtensionHost.ts`)

- `TauriRelaySocket` implements `ISocket`: `exthost://data/{id}` → `onData`,
  `write()` → `exthost_write`, `exthost://exit/{id}` → `onClose`/`onEnd`.
- `TauriExtensionHost implements IExtensionHost`:
  1. `start_extension_host` → id,
  2. subscribe the relay socket,
  3. `new PersistentProtocol({ socket })` + `sendResume()`,
  4. handshake: wait `Ready` → send init data → wait `Initialized`.

Access to Tauri's JS API is via the injected global `window.__TAURI__`
(`core.invoke` + `event.listen`), guarded by `getTauri()`.

## 7. Open issues / TODO

- **`withGlobalTauri`**: `tauriExtensionHost.ts` reads `window.__TAURI__`, which
  only exists when `app.withGlobalTauri: true` is set in
  `src-tauri/tauri.conf.json`. That file is owned by the shell agent; it is not
  enabled yet. Alternative: bundle `@tauri-apps/api` into the workbench build.
- **Init data is incomplete.** `_createInitData()` currently forwards only the
  extensions snapshot. The full `IExtensionHostInitData` (product/version,
  workspace, telemetry info, logs location, `uiKind`, env URIs, …) must be
  assembled the way `localProcessExtensionHost._createExtHostInitData` does,
  once the *browser* environment/product/userDataProfile services are wired into
  the Tauri build. Until then the host will boot but may mis-report environment.
- **No inspector port.** `getInspectPort()`/`enableInspectPort()` are stubs. The
  Node `--inspect` banner is only logged (stdout/stderr pumped to `tracing`);
  parse it and surface `IExtensionInspectInfo` for extension debugging.
- **No reconnection.** `PersistentProtocol` supports reconnection
  (`beginAcceptReconnection`), but the relay accepts a single connection and
  tears down on socket close. Reconnection/grace-time (`VSCODE_RECONNECTION_*`)
  is not implemented; a renderer reload currently means a fresh host.
- **Write backpressure.** `exthost_write` pushes into an unbounded mpsc channel;
  a slow socket could grow memory. Consider a bounded channel + async `write()`.
- **Dangerous env stripping.** Electron calls `removeDangerousEnvVariables` and
  deletes `VSCODE_CODE_CACHE_PATH` for dev hosts. The Rust side forwards env
  as-is plus caller overrides; replicate the sanitisation.
- **Shutdown grace / force kill.** Electron force-kills a host that reports exit
  but keeps running (issue #194477) and awaits graceful exit on quit. Only a
  basic `start_kill` + `wait` is implemented here.
- **stdout/stderr forwarding.** Raw output is logged in Rust, not forwarded to
  the renderer DevTools console the way Electron does (debounced grouping,
  inspector-URL detection). Not required for function, useful for parity.
