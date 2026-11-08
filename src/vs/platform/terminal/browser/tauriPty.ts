/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import { VSBuffer } from '../../../base/common/buffer.js';
import { Emitter } from '../../../base/common/event.js';
import { Disposable, toDisposable } from '../../../base/common/lifecycle.js';
import {
	IProcessDataEvent,
	IProcessProperty,
	IProcessPropertyMap,
	IProcessReadyEvent,
	ITerminalChildProcess,
	ITerminalLaunchError,
	ITerminalLaunchResult,
	ProcessPropertyType,
} from '../common/terminal.js';

/**
 * Minimal subset of Tauri's internal bridge that is injected into the webview
 * global scope as `window.__TAURI_INTERNALS__`. We talk to the Rust `pty`
 * service (`src-tauri/src/services/pty.rs`) through it directly instead of the
 * `@tauri-apps/api` package so the workbench keeps zero npm dependencies on
 * Tauri.
 */
interface ITauriInternals {
	/**
	 * Invokes a `#[tauri::command]`. Returns a promise resolving to the
	 * command's `Ok` payload or rejecting with its `Err` payload.
	 */
	invoke<T = unknown>(cmd: string, args?: Record<string, unknown>): Promise<T>;
	/**
	 * Subscribes to a Tauri event. Resolves to an unlisten function.
	 */
	transformCallback?(callback: (response: unknown) => void, once?: boolean): number;
}

declare global {
	interface Window {
		__TAURI_INTERNALS__?: ITauriInternals;
	}
}

/** Payload shape emitted by the Rust side on `pty://exit/{id}`. */
interface IPtyExitPayload {
	id: number;
	exit_code: number | null;
}

/** Options forwarded to the Rust `pty_spawn` command. */
interface IPtySpawnOptions {
	shell: string | null;
	args: string[];
	cwd: string | null;
	env: [string, string][];
	cols: number;
	rows: number;
}

/**
 * Returns the Tauri internals bridge, throwing a descriptive error when the
 * workbench is not running inside the Tauri (code-rs) shell.
 */
function getTauri(): ITauriInternals {
	const tauri = window.__TAURI_INTERNALS__;
	if (!tauri || typeof tauri.invoke !== 'function') {
		throw new Error('tauriPty: window.__TAURI_INTERNALS__ is not available; not running inside the Tauri shell');
	}
	return tauri;
}

/**
 * Invokes a Tauri command through the internals bridge.
 */
function invoke<T = unknown>(cmd: string, args?: Record<string, unknown>): Promise<T> {
	return getTauri().invoke<T>(cmd, args);
}

/**
 * Subscribes to a Tauri event by name.
 *
 * Tauri exposes event listening on the internals bridge in a way that has
 * shifted between betas; to stay resilient we probe the known shapes
 * (`__TAURI_INTERNALS__.invoke('plugin:event|listen', …)`) and fall back to the
 * higher-level `window.__TAURI__.event.listen` when present. The returned
 * promise resolves to an unlisten function.
 */
async function listen(
	event: string,
	handler: (payload: unknown) => void
): Promise<() => void> {
	// Preferred: the public event API when the `@tauri-apps/api` global is
	// exposed (it re-uses the internals bridge under the hood).
	const pub = (window as unknown as {
		__TAURI__?: { event?: { listen?: (event: string, cb: (e: { payload: unknown }) => void) => Promise<unknown> } };
	}).__TAURI__;
	if (pub?.event?.listen) {
		const unlisten = await pub.event.listen(event, (e: { payload: unknown }) => handler(e.payload));
		return typeof unlisten === 'function' ? unlisten as () => void : () => { };
	}

	// Fallback: drive the event plugin directly through the internals bridge.
	const tauri = getTauri();
	if (typeof tauri.transformCallback !== 'function') {
		throw new Error('tauriPty: no usable Tauri event listening API found');
	}
	const callbackId = tauri.transformCallback((response: unknown) => {
		handler((response as { payload: unknown })?.payload);
	}, false);
	const unlistenFn = await tauri.invoke<number>('plugin:event|listen', {
		event,
		target: { kind: 'Any' },
		handler: callbackId,
	});
	return () => {
		void tauri.invoke('plugin:event|unlisten', { event, eventId: unlistenFn });
	};
}

/**
 * An {@link ITerminalChildProcess} backed by the Rust `pty` service running in
 * the Tauri shell.
 *
 * Output arrives over the `pty://data/{id}` event and is surfaced through
 * {@link onProcessData}; process exit arrives over `pty://exit/{id}` and is
 * surfaced through {@link onProcessExit}. Input, resize and kill map onto the
 * `pty_write`, `pty_resize` and `pty_kill` commands respectively.
 *
 * Reconnection/persistence is not supported (the pty host lives in-process), so
 * {@link shouldPersist} is `false` and {@link id} is `0` until the backend id
 * is known — see the constructor note below.
 */
export class TauriPty extends Disposable implements ITerminalChildProcess {
	/**
	 * Terminal-protocol id. This is the workbench-assigned persistence id; since
	 * this backend does not persist across reloads it is always `0`. The Rust
	 * session id is tracked separately in {@link _ptyId}.
	 */
	readonly id = 0;
	readonly shouldPersist = false;

	/** The backend (Rust) session id, assigned once {@link start} succeeds. */
	private _ptyId: number | undefined;

	/** Unlisten callbacks for the Tauri event subscriptions. */
	private _unlistenData: (() => void) | undefined;
	private _unlistenExit: (() => void) | undefined;

	/** Tracks whether an exit has already been reported to avoid duplicates. */
	private _exited = false;

	private readonly _onProcessData = this._register(new Emitter<IProcessDataEvent | string>());
	readonly onProcessData = this._onProcessData.event;
	private readonly _onProcessReady = this._register(new Emitter<IProcessReadyEvent>());
	readonly onProcessReady = this._onProcessReady.event;
	private readonly _onDidChangeProperty = this._register(new Emitter<IProcessProperty>());
	readonly onDidChangeProperty = this._onDidChangeProperty.event;
	private readonly _onProcessExit = this._register(new Emitter<number | undefined>());
	readonly onProcessExit = this._onProcessExit.event;

	constructor(
		private readonly _shell: string | undefined,
		private readonly _args: string[],
		private _cwd: string | undefined,
		private readonly _env: { [key: string]: string | null | undefined },
		private _cols: number,
		private _rows: number,
	) {
		super();
		this._register(toDisposable(() => {
			this._unlistenData?.();
			this._unlistenExit?.();
		}));
	}

	async start(): Promise<ITerminalLaunchError | ITerminalLaunchResult | undefined> {
		try {
			const env: [string, string][] = Object.entries(this._env)
				.filter((entry): entry is [string, string] => typeof entry[1] === 'string')
				.map(([k, v]) => [k, v as string]);

			const options: IPtySpawnOptions = {
				shell: this._shell ?? null,
				args: this._args,
				cwd: this._cwd ?? null,
				env,
				cols: Math.max(1, Math.floor(this._cols) || 1),
				rows: Math.max(1, Math.floor(this._rows) || 1),
			};

			const ptyId = await invoke<number>('pty_spawn', { options });
			this._ptyId = ptyId;

			// Wire up output/exit streams before announcing readiness so no early
			// data is lost.
			this._unlistenData = await listen(`pty://data/${ptyId}`, payload => {
				if (typeof payload === 'string') {
					this._onProcessData.fire(payload);
				}
			});
			this._unlistenExit = await listen(`pty://exit/${ptyId}`, payload => {
				const exit = payload as IPtyExitPayload | undefined;
				this._fireExit(exit?.exit_code ?? undefined);
			});

			// The backend does not (yet) report the real pid/cwd back on spawn;
			// surface what we know. pid 0 means "unknown" to the workbench.
			this._onProcessReady.fire({
				pid: 0,
				cwd: this._cwd ?? '',
				windowsPty: undefined,
			});

			return undefined;
		} catch (err) {
			return { message: `Failed to start terminal process: ${err instanceof Error ? err.message : String(err)}` };
		}
	}

	private _fireExit(code: number | undefined): void {
		if (this._exited) {
			return;
		}
		this._exited = true;
		this._onProcessExit.fire(code);
		this._unlistenData?.();
		this._unlistenExit?.();
		this._unlistenData = undefined;
		this._unlistenExit = undefined;
	}

	shutdown(immediate: boolean): void {
		// Both immediate and graceful shutdown map onto killing the backend pty;
		// the Rust reader thread flushes remaining output before emitting exit.
		void this._kill();
		if (immediate) {
			this._fireExit(undefined);
		}
	}

	private async _kill(): Promise<void> {
		if (this._ptyId === undefined || this._exited) {
			return;
		}
		try {
			await invoke('pty_kill', { id: this._ptyId });
		} catch {
			// Session may already be gone; ignore.
		}
	}

	input(data: string): void {
		if (this._ptyId === undefined) {
			return;
		}
		void invoke('pty_write', { id: this._ptyId, data }).catch(() => { /* session gone */ });
	}

	sendSignal(signal: string): void {
		// The current Rust backend only exposes kill; treat any signal as a kill
		// request. A richer signal API can be added to pty.rs later.
		void signal;
		void this._kill();
	}

	async processBinary(data: string): Promise<void> {
		// The workbench sends binary as a latin1 string; forward verbatim. The
		// Rust side writes the bytes unchanged.
		if (this._ptyId === undefined) {
			return;
		}
		await invoke('pty_write', { id: this._ptyId, data });
	}

	resize(cols: number, rows: number): void {
		this._cols = cols;
		this._rows = rows;
		if (this._ptyId === undefined) {
			return;
		}
		void invoke('pty_resize', {
			id: this._ptyId,
			cols: Math.max(1, Math.floor(cols) || 1),
			rows: Math.max(1, Math.floor(rows) || 1),
		}).catch(() => { /* session gone */ });
	}

	clearBuffer(): void {
		// No server-side scrollback buffer to clear in this backend.
	}

	acknowledgeDataEvent(_charCount: number): void {
		// Flow control is not implemented in the in-process backend; data is not
		// paused so acknowledgements are a no-op.
	}

	async setUnicodeVersion(_version: '6' | '11'): Promise<void> {
		// The pty backend is encoding-agnostic (raw bytes); nothing to do.
	}

	async getInitialCwd(): Promise<string> {
		return this._cwd ?? '';
	}

	async getCwd(): Promise<string> {
		// cwd tracking is not yet reported by the backend; return the initial
		// cwd as a best effort.
		return this._cwd ?? '';
	}

	async refreshProperty<T extends ProcessPropertyType>(type: T): Promise<IProcessPropertyMap[T]> {
		switch (type) {
			case ProcessPropertyType.Cwd:
				return (this._cwd ?? '') as IProcessPropertyMap[T];
			case ProcessPropertyType.InitialCwd:
				return (this._cwd ?? '') as IProcessPropertyMap[T];
			case ProcessPropertyType.Title:
				return (this._shell ?? '') as IProcessPropertyMap[T];
			case ProcessPropertyType.HasChildProcesses:
				return true as IProcessPropertyMap[T];
			default:
				return undefined as IProcessPropertyMap[T];
		}
	}

	async updateProperty<T extends ProcessPropertyType>(type: T, value: IProcessPropertyMap[T]): Promise<void> {
		if (type === ProcessPropertyType.Cwd && typeof value === 'string') {
			this._cwd = value;
		}
		// Other properties have no backend representation yet.
	}
}

/**
 * Lists the pty sessions currently tracked by the Rust backend. Useful for
 * diagnostics / process-explorer style UIs. Mirrors the `pty_list` command.
 */
export interface ITauriPtyProcessInfo {
	id: number;
	pid: number | null;
	shell: string;
	cwd: string | null;
	cols: number;
	rows: number;
}

export function listTauriPtyProcesses(): Promise<ITauriPtyProcessInfo[]> {
	return invoke<ITauriPtyProcessInfo[]>('pty_list');
}

/**
 * Convenience helper mirroring the `create(shell, args, cwd, env, cols, rows)`
 * signature exposed by the backend.
 */
export function createTauriPty(
	shell: string | undefined,
	args: string[],
	cwd: string | undefined,
	env: { [key: string]: string | null | undefined },
	cols: number,
	rows: number,
): TauriPty {
	return new TauriPty(shell, args, cwd, env, cols, rows);
}
