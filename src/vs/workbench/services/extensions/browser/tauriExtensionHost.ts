/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

//
// Tauri extension host (code-rs)
// ------------------------------
//
// This is the Tauri analogue of
// `electron-browser/localProcessExtensionHost.ts`. Instead of launching the
// extension host as an Electron *utility process* and handing it a
// `MessagePort`, the Rust shell (`src-tauri/src/services/extension_host.rs`)
// spawns the Node extension host as a child process and exposes a raw byte
// relay over:
//
//   * command `start_extension_host(options)` -> opaque numeric host id
//   * command `exthost_write(id, bytesBase64)`  (webview -> node)
//   * command `stop_extension_host(id)`
//   * event   `exthost://data/{id}`     base64 bytes  (node -> webview)
//   * event   `exthost://started/{id}`  `{ id, pid }`
//   * event   `exthost://exit/{id}`     `{ id, code, signal }`
//
// The Node side wraps its end of the pipe in a `PersistentProtocol`
// (see `extensionHostProcess.ts`, the `ExtHostConnectionType.IPC` branch, which
// calls `sendResume()`). We therefore drive a matching `PersistentProtocol` on
// this side over an `ISocket` that is backed by the relay. The ack/keep-alive
// framing is handled entirely by the two `PersistentProtocol` endpoints — the
// Rust bridge only moves bytes.
//
// NOTE on Tauri API access: the compiled web workbench does not bundle
// `@tauri-apps/api`. We access the APIs through the global that Tauri injects
// when `withGlobalTauri: true` is set in `tauri.conf.json` (owned by the shell
// agent — see docs/rust-rewrite/EXTHOST.md). We probe the global lazily and
// fail with a clear error if it is missing.
//

import { timeout } from '../../../../base/common/async.js';
import { decodeBase64, encodeBase64, VSBuffer } from '../../../../base/common/buffer.js';
import { CancellationError } from '../../../../base/common/errors.js';
import { Emitter, Event } from '../../../../base/common/event.js';
import { Disposable, IDisposable, toDisposable } from '../../../../base/common/lifecycle.js';
import { IMessagePassingProtocol } from '../../../../base/parts/ipc/common/ipc.js';
import {
	ISocket,
	PersistentProtocol,
	SocketCloseEvent,
	SocketCloseEventType,
	SocketDiagnosticsEventType,
} from '../../../../base/parts/ipc/common/ipc.net.js';
import { createMessageOfType, isMessageOfType, MessageType } from '../common/extensionHostProtocol.js';
import { ExtensionRunningLocation } from '../common/extensionRunningLocation.js';
import {
	ExtensionHostExtensions,
	ExtensionHostStartup,
	IExtensionHost,
	IExtensionInspectInfo,
} from '../common/extensions.js';

//#region Tauri global bridge

/**
 * Minimal shape of the subset of the Tauri JS API we use. Matches
 * `@tauri-apps/api`'s `core.invoke` and `event.listen`.
 */
interface ITauriBridge {
	invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T>;
	listen<T>(event: string, handler: (event: { payload: T }) => void): Promise<() => void>;
}

interface ITauriGlobal {
	core?: { invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> };
	// Older/global builds expose invoke at the root as well.
	invoke?<T>(cmd: string, args?: Record<string, unknown>): Promise<T>;
	event?: { listen<T>(event: string, handler: (event: { payload: T }) => void): Promise<() => void> };
}

function getTauri(): ITauriBridge {
	const g = (globalThis as unknown as { __TAURI__?: ITauriGlobal }).__TAURI__;
	const invokeFn = g?.core?.invoke ?? g?.invoke;
	const invokeThis = g?.core?.invoke ? g.core : g;
	const listenFn = g?.event?.listen;
	if (!invokeFn || !listenFn || !g) {
		throw new Error(
			`Tauri API not available on window.__TAURI__. ` +
			`Set "app.withGlobalTauri": true in src-tauri/tauri.conf.json.`
		);
	}
	return {
		invoke: invokeFn.bind(invokeThis),
		listen: listenFn.bind(g.event),
	};
}

//#endregion

//#region ISocket over the Tauri relay

/**
 * Options forwarded verbatim to the Rust `start_extension_host` command. Kept
 * intentionally loose: path/arg resolution happens Rust-side.
 */
export interface ITauriExtensionHostOptions {
	readonly nodePath?: string;
	readonly bootstrapPath?: string;
	readonly args?: string[];
	readonly execArgv?: string[];
	readonly env?: Record<string, string>;
}

export interface ITauriExtensionHostDataProvider {
	getInitData(): Promise<{ readonly extensions: ExtensionHostExtensions }>;
}

/**
 * An `ISocket` whose bytes are relayed to/from the Node extension host by the
 * Rust bridge. Reads arrive as base64 `exthost://data/{id}` events; writes go
 * out via the `exthost_write` command.
 */
class TauriRelaySocket extends Disposable implements ISocket {

	private readonly _onData = this._register(new Emitter<VSBuffer>());
	private readonly _onClose = this._register(new Emitter<SocketCloseEvent>());
	private readonly _onEnd = this._register(new Emitter<void>());

	private _closed = false;
	private _unlistenData: (() => void) | null = null;
	private _unlistenExit: (() => void) | null = null;

	constructor(
		private readonly _tauri: ITauriBridge,
		private readonly _id: number,
	) {
		super();
		this._register(toDisposable(() => {
			this._unlistenData?.();
			this._unlistenExit?.();
		}));
	}

	/**
	 * Subscribe to the relay events. Must be awaited before the protocol starts
	 * sending so we do not miss the first chunk(s).
	 */
	async connect(): Promise<void> {
		this._unlistenData = await this._tauri.listen<string>(`exthost://data/${this._id}`, (evt) => {
			if (this._closed) {
				return;
			}
			const buf = decodeBase64(evt.payload);
			this.traceSocketEvent(SocketDiagnosticsEventType.Read, buf);
			this._onData.fire(buf);
		});
		this._unlistenExit = await this._tauri.listen<unknown>(`exthost://exit/${this._id}`, () => {
			this._fireClose(undefined);
		});
	}

	private _fireClose(error: Error | undefined): void {
		if (this._closed) {
			return;
		}
		this._closed = true;
		this._onEnd.fire();
		this._onClose.fire({
			type: SocketCloseEventType.NodeSocketCloseEvent,
			hadError: !!error,
			error,
		});
	}

	onData(listener: (e: VSBuffer) => void): IDisposable {
		return this._onData.event(listener);
	}

	onClose(listener: (e: SocketCloseEvent) => void): IDisposable {
		return this._onClose.event(listener);
	}

	onEnd(listener: () => void): IDisposable {
		return this._onEnd.event(listener);
	}

	write(buffer: VSBuffer): void {
		if (this._closed) {
			return;
		}
		this.traceSocketEvent(SocketDiagnosticsEventType.Write, buffer);
		const b64 = encodeBase64(buffer);
		// Fire-and-forget: ordering is preserved because the Rust side funnels
		// writes through a single FIFO channel, and `invoke` promises resolve
		// in submission order for a given command queue.
		this._tauri.invoke<void>('exthost_write', { id: this._id, bytes: b64 }).catch((err) => {
			// A write failure means the pipe is gone; treat as a close.
			this._fireClose(err instanceof Error ? err : new Error(String(err)));
		});
	}

	end(): void {
		// There is no half-close over the relay; a full stop happens via
		// `stop_extension_host`. Mark closed so further writes are dropped.
		this._fireClose(undefined);
	}

	drain(): Promise<void> {
		// Writes are forwarded immediately; nothing is buffered locally.
		return Promise.resolve();
	}

	traceSocketEvent(type: SocketDiagnosticsEventType, data?: VSBuffer | Uint8Array | ArrayBuffer | ArrayBufferView | unknown): void {
		// Diagnostics are disabled by default (SocketDiagnostics.enableDiagnostics
		// === false); keep this a cheap no-op to satisfy the ISocket contract.
		void type;
		void data;
	}
}

//#endregion

/**
 * `IExtensionHost` backed by a Node extension host child process launched and
 * relayed by the Tauri (Rust) shell.
 */
export class TauriExtensionHost extends Disposable implements IExtensionHost {

	public pid: number | null = null;
	public readonly remoteAuthority = null;
	public extensions: ExtensionHostExtensions | null = null;

	private readonly _onExit = this._register(new Emitter<[number, string | null]>());
	public readonly onExit: Event<[number, string | null]> = this._onExit.event;

	private _terminating = false;
	private _hostId: number | null = null;
	private _protocol: IMessagePassingProtocol | null = null;
	private _messageProtocol: Promise<IMessagePassingProtocol> | null = null;

	constructor(
		public readonly runningLocation: ExtensionRunningLocation,
		public readonly startup: ExtensionHostStartup,
		private readonly _initDataProvider: ITauriExtensionHostDataProvider,
		private readonly _options: ITauriExtensionHostOptions = {},
	) {
		super();
	}

	public override dispose(): void {
		if (!this._terminating) {
			this._terminating = true;
			// Best-effort: ask the Rust bridge to stop the child.
			if (this._hostId !== null) {
				try {
					getTauri().invoke<void>('stop_extension_host', { id: this._hostId }).catch(() => { /* ignore */ });
				} catch {
					// Tauri not available (shouldn't happen if we got this far).
				}
			}
		}
		super.dispose();
		this._protocol = null;
		this._messageProtocol = null;
	}

	public start(): Promise<IMessagePassingProtocol> {
		if (this._terminating) {
			throw new CancellationError();
		}
		if (!this._messageProtocol) {
			this._messageProtocol = this._start();
		}
		return this._messageProtocol;
	}

	private async _start(): Promise<IMessagePassingProtocol> {
		const tauri = getTauri();

		// 1) Create the relay socket and subscribe to its events *before* asking
		//    the Rust side to spawn Node, so no bytes are missed.
		//    We don't yet know the id, so we start the host first to get it, but
		//    the Rust side only accepts the pipe connection asynchronously and
		//    emits data afterwards — so subscribing right after start is safe.
		const hostId = await tauri.invoke<number>('start_extension_host', {
			options: {
				nodePath: this._options.nodePath,
				bootstrapPath: this._options.bootstrapPath,
				// `VSCODE_ESM_ENTRYPOINT` / `VSCODE_EXTHOST_IPC_HOOK` are set
				// Rust-side; callers only supply extra args/env.
				args: this._options.args ?? ['--skipWorkspaceStorageLock'],
				execArgv: this._options.execArgv ?? ['--inspect-port=0'],
				env: this._options.env ?? {},
			},
		});
		this._hostId = hostId;

		const socket = this._register(new TauriRelaySocket(tauri, hostId));
		await socket.connect();

		// Surface process exit to IExtensionHost consumers.
		const unlistenExit = await tauri.listen<{ code: number | null; signal: string | null }>(
			`exthost://exit/${hostId}`,
			(evt) => this._onProcessExit(evt.payload?.code ?? 0, evt.payload?.signal ?? null),
		);
		this._register(toDisposable(() => unlistenExit()));

		// 2) Build the PersistentProtocol over the relay socket. The Node side
		//    calls sendResume(); we do the same so both ends agree on the
		//    reconnection/ack state machine.
		const protocol = new PersistentProtocol({ socket });
		protocol.sendResume();
		this._protocol = protocol;

		// 3) Handshake: wait for Ready, send init data, wait for Initialized.
		await this._performHandshake(protocol);

		return protocol;
	}

	private _performHandshake(protocol: IMessagePassingProtocol): Promise<void> {
		// Mirrors localProcessExtensionHost._performHandshake.
		return new Promise<void>((resolve, reject) => {
			let handle: Timeout | undefined;
			const install = () => {
				handle = setTimeout(() => reject(new Error('The local extension host took longer than 60s to connect.')), 60_000);
			};
			const uninstall = () => clearTimeout(handle);

			install();

			const disposable = protocol.onMessage(msg => {
				if (isMessageOfType(msg, MessageType.Ready)) {
					// Extension host is ready; send init data.
					uninstall();
					this._createInitData().then(data => {
						install();
						protocol.send(VSBuffer.fromString(JSON.stringify(data)));
					}, reject);
					return;
				}
				if (isMessageOfType(msg, MessageType.Initialized)) {
					uninstall();
					disposable.dispose();
					resolve();
					return;
				}
				console.error('[TauriExtensionHost] unexpected message during handshake: ', msg);
			});
		});
	}

	private async _createInitData(): Promise<object> {
		const initData = await this._initDataProvider.getInitData();
		this.extensions = initData.extensions;
		// The concrete IExtensionHostInitData payload is assembled by the
		// workbench's data provider integration. We forward the extensions
		// snapshot and let the provider enrich it. See EXTHOST.md "open issues":
		// init data construction mirrors localProcessExtensionHost._createExtHostInitData
		// and will be wired fully once the browser environment service exposes
		// the required fields in the Tauri build.
		return {
			extensions: this.extensions.toSnapshot(),
		};
	}

	private _onProcessExit(code: number, signal: string | null): void {
		if (this._terminating) {
			return;
		}
		this._onExit.fire([code, signal]);
	}

	public getInspectPort(): IExtensionInspectInfo | undefined {
		// Inspector port discovery (parsing the Node `--inspect` banner from
		// stdout) is a TODO; the Rust bridge currently logs stdout/stderr only.
		return undefined;
	}

	public async enableInspectPort(): Promise<boolean> {
		return false;
	}

	public async disconnect(): Promise<void> {
		this._terminating = true;
		// Send Terminate so the extension host can run deactivation handlers.
		if (this._protocol) {
			try {
				this._protocol.send(createMessageOfType(MessageType.Terminate));
				await timeout(100);
			} catch {
				// ignore — host may already be gone
			}
		}
		if (this._hostId !== null) {
			try {
				await getTauri().invoke<void>('stop_extension_host', { id: this._hostId });
			} catch {
				// ignore
			}
		}
	}
}
