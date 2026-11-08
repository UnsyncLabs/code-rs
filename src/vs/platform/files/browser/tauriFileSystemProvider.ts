/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import { URI } from '../../../base/common/uri.js';
import { Emitter, Event } from '../../../base/common/event.js';
import { Disposable, IDisposable, toDisposable } from '../../../base/common/lifecycle.js';
import { isLinux } from '../../../base/common/platform.js';
import {
	createFileSystemProviderError,
	FileChangeType,
	FileSystemProviderCapabilities,
	FileSystemProviderErrorCode,
	FileType,
	IFileChange,
	IFileDeleteOptions,
	IFileOverwriteOptions,
	IFileSystemProviderWithFileReadWriteCapability,
	IFileWriteOptions,
	IStat,
	IWatchOptions
} from '../common/files.js';

/**
 * Minimal shape of the Tauri `invoke` bridge the host injects on `window`.
 * We call it directly to avoid taking an npm dependency on `@tauri-apps/api`.
 */
interface ITauriInternals {
	invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T>;
}

/**
 * Shape of the `@tauri-apps/api/event` `listen` helper when available on the
 * internals global. We fall back to a DOM event bridge when it is not present.
 */
type TauriUnlistenFn = () => void;
type TauriEventCallback<T> = (event: { payload: T }) => void;

interface ITauriWindow {
	__TAURI_INTERNALS__?: ITauriInternals & {
		// Some Tauri 2 builds expose an event transport here; we probe for it
		// and gracefully fall back if it is missing.
		event?: {
			listen<T>(event: string, handler: TauriEventCallback<T>): Promise<TauriUnlistenFn>;
		};
	};
}

/**
 * The error payload the Rust `fs` service returns: `{ code, message }` where
 * `code` is a `FileSystemProviderErrorCode` string value.
 */
interface ITauriProviderError {
	code: FileSystemProviderErrorCode;
	message: string;
}

/**
 * The change payload emitted on the `fs://change` Tauri event, matching the
 * Rust `FileChange` struct and VS Code's `IFileChange` (sans `resource`
 * being a URI — it arrives as an absolute filesystem path string).
 */
interface ITauriFileChange {
	resource: string;
	type: 'added' | 'updated' | 'deleted';
	cId?: number;
}

const FS_CHANGE_EVENT = 'fs://change';

/**
 * A `IFileSystemProvider` implementation backed by native Rust `tauri::command`
 * handlers (see `src-tauri/src/services/fs.rs` and `watcher.rs`). Used in place
 * of the Node disk provider when the workbench runs inside the Tauri shell.
 *
 * File contents cross the IPC boundary base64-encoded so arbitrary bytes
 * survive JSON transport.
 */
export class TauriFileSystemProvider extends Disposable implements IFileSystemProviderWithFileReadWriteCapability {

	//#region Capabilities

	readonly onDidChangeCapabilities = Event.None;

	private _capabilities: FileSystemProviderCapabilities | undefined;
	get capabilities(): FileSystemProviderCapabilities {
		if (!this._capabilities) {
			this._capabilities =
				FileSystemProviderCapabilities.FileReadWrite |
				FileSystemProviderCapabilities.FileFolderCopy |
				FileSystemProviderCapabilities.FileAtomicWrite |
				FileSystemProviderCapabilities.FileAtomicRename |
				FileSystemProviderCapabilities.Trash |
				FileSystemProviderCapabilities.FileRealpath;

			if (isLinux) {
				this._capabilities |= FileSystemProviderCapabilities.PathCaseSensitive;
			}
		}

		return this._capabilities;
	}

	//#endregion

	//#region Events

	private readonly _onDidChangeFile = this._register(new Emitter<readonly IFileChange[]>());
	readonly onDidChangeFile = this._onDidChangeFile.event;

	private readonly _onDidWatchError = this._register(new Emitter<string>());
	readonly onDidWatchError = this._onDidWatchError.event;

	//#endregion

	/**
	 * Monotonic id for correlating watch requests. Each `watch()` call allocates
	 * one so incoming `fs://change` events can be matched to the originating
	 * request via `cId`.
	 */
	private watchRequestId = 0;

	/**
	 * Pending frontend watch disposables keyed by the native watcher id (the
	 * number returned by the Rust `watch` command). Resolved asynchronously.
	 */
	private readonly nativeWatchers = new Map<number, number /* correlationId */>();

	private tauriEventUnlisten: TauriUnlistenFn | undefined;
	private domEventDisposable: IDisposable | undefined;

	constructor() {
		super();
		this.registerChangeListener();
	}

	private get internals(): ITauriInternals {
		const tauri = (globalThis as unknown as ITauriWindow).__TAURI_INTERNALS__;
		if (!tauri) {
			throw createFileSystemProviderError(
				'Tauri internals are not available on this window',
				FileSystemProviderErrorCode.Unavailable
			);
		}
		return tauri;
	}

	private invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
		return this.internals.invoke<T>(cmd, args);
	}

	/**
	 * Wrap a rejected native call, converting the `{ code, message }` payload
	 * into a VS Code `FileSystemProviderError` so workbench code sees the
	 * expected error codes.
	 */
	private toProviderError(error: unknown): Error {
		const candidate = error as Partial<ITauriProviderError> | undefined;
		if (candidate && typeof candidate.code === 'string') {
			return createFileSystemProviderError(
				candidate.message ?? candidate.code,
				candidate.code as FileSystemProviderErrorCode
			);
		}
		return createFileSystemProviderError(
			error instanceof Error ? error : String(error),
			FileSystemProviderErrorCode.Unknown
		);
	}

	private async call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
		try {
			return await this.invoke<T>(cmd, args);
		} catch (error) {
			throw this.toProviderError(error);
		}
	}

	//#region File Metadata

	async stat(resource: URI): Promise<IStat> {
		const result = await this.call<{ type: number; mtime: number; ctime: number; size: number; permissions?: number }>('stat', {
			path: this.toPath(resource)
		});
		return {
			type: result.type as FileType,
			mtime: result.mtime,
			ctime: result.ctime,
			size: result.size,
			permissions: result.permissions as IStat['permissions']
		};
	}

	async readdir(resource: URI): Promise<[string, FileType][]> {
		const entries = await this.call<{ name: string; type: number }[]>('read_dir', {
			path: this.toPath(resource)
		});
		return entries.map(entry => [entry.name, entry.type as FileType]);
	}

	//#endregion

	//#region File Reading/Writing

	async readFile(resource: URI): Promise<Uint8Array> {
		const base64 = await this.call<string>('read_file', { path: this.toPath(resource) });
		return decodeBase64ToBytes(base64);
	}

	async writeFile(resource: URI, content: Uint8Array, opts: IFileWriteOptions): Promise<void> {
		await this.call<void>('write_file', {
			path: this.toPath(resource),
			contents: encodeBytesToBase64(content),
			options: {
				create: opts.create,
				overwrite: opts.overwrite,
				atomic: opts.atomic !== false && opts.atomic !== undefined
			}
		});
	}

	//#endregion

	//#region Move/Copy/Delete/Create

	async mkdir(resource: URI): Promise<void> {
		await this.call<void>('mkdir', { path: this.toPath(resource) });
	}

	async delete(resource: URI, opts: IFileDeleteOptions): Promise<void> {
		await this.call<void>('delete', {
			path: this.toPath(resource),
			options: {
				recursive: opts.recursive,
				useTrash: opts.useTrash
			}
		});
	}

	async rename(from: URI, to: URI, opts: IFileOverwriteOptions): Promise<void> {
		await this.call<void>('rename', {
			from: this.toPath(from),
			to: this.toPath(to),
			options: { overwrite: opts.overwrite }
		});
	}

	async copy(from: URI, to: URI, opts: IFileOverwriteOptions): Promise<void> {
		await this.call<void>('copy', {
			from: this.toPath(from),
			to: this.toPath(to),
			options: { overwrite: opts.overwrite }
		});
	}

	async realpath(resource: URI): Promise<string> {
		// The optional `IFileRealpathOptions` second parameter is accepted by
		// the capability interface but carries no options we act on natively.
		return this.call<string>('realpath', { path: this.toPath(resource) });
	}

	//#endregion

	//#region Watching

	watch(resource: URI, opts: IWatchOptions): IDisposable {
		const correlationId = opts.correlationId ?? ++this.watchRequestId;

		let nativeId: number | undefined;
		let disposed = false;

		this.call<number>('watch', {
			path: this.toPath(resource),
			options: {
				recursive: opts.recursive,
				excludes: opts.excludes ?? [],
				correlationId
			}
		}).then(id => {
			if (disposed) {
				// Watch was disposed before the native call resolved; tear down.
				void this.call<void>('unwatch', { id }).catch(() => undefined);
				return;
			}
			nativeId = id;
			this.nativeWatchers.set(id, correlationId);
		}, error => {
			this._onDidWatchError.fire(String(error));
		});

		return toDisposable(() => {
			disposed = true;
			if (nativeId !== undefined) {
				this.nativeWatchers.delete(nativeId);
				void this.call<void>('unwatch', { id: nativeId }).catch(() => undefined);
			}
		});
	}

	/**
	 * Subscribe to the `fs://change` Tauri event. Prefers the native event
	 * transport (`__TAURI_INTERNALS__.event.listen`) when present; otherwise
	 * falls back to a DOM `CustomEvent` bridge that a thin preload can forward.
	 */
	private registerChangeListener(): void {
		const tauri = (globalThis as unknown as ITauriWindow).__TAURI_INTERNALS__;

		if (tauri?.event?.listen) {
			tauri.event.listen<ITauriFileChange>(FS_CHANGE_EVENT, event => {
				this.handleNativeChange(event.payload);
			}).then(unlisten => {
				this.tauriEventUnlisten = unlisten;
			}, error => {
				this._onDidWatchError.fire(String(error));
			});
			this._register(toDisposable(() => this.tauriEventUnlisten?.()));
			return;
		}

		// Fallback: Tauri forwards events to the webview as a DOM event on
		// `window` in some configurations. Listen defensively.
		const handler = (evt: globalThis.Event) => {
			const detail = (evt as CustomEvent<ITauriFileChange>).detail;
			if (detail) {
				this.handleNativeChange(detail);
			}
		};
		globalThis.addEventListener?.(FS_CHANGE_EVENT, handler as EventListener);
		this.domEventDisposable = toDisposable(() =>
			globalThis.removeEventListener?.(FS_CHANGE_EVENT, handler as EventListener)
		);
		this._register(this.domEventDisposable);
	}

	private handleNativeChange(payload: ITauriFileChange): void {
		const change: IFileChange = {
			resource: this.toUri(payload.resource),
			type: toFileChangeType(payload.type),
			cId: payload.cId
		};
		this._onDidChangeFile.fire([change]);
	}

	//#endregion

	//#region Path <-> URI

	private toPath(resource: URI): string {
		// The native side works with OS filesystem paths. `URI.fsPath` yields a
		// platform-correct path (backslashes on Windows, drive letters, etc.).
		return resource.fsPath;
	}

	private toUri(path: string): URI {
		return URI.file(path);
	}

	//#endregion
}

function toFileChangeType(type: ITauriFileChange['type']): FileChangeType {
	switch (type) {
		case 'added': return FileChangeType.ADDED;
		case 'deleted': return FileChangeType.DELETED;
		case 'updated':
		default:
			return FileChangeType.UPDATED;
	}
}

/**
 * Decode a base64 string to raw bytes. Uses VSBuffer so the result is the
 * `Uint8Array` the file service expects, and works in both DOM and worker
 * contexts (atob is available in both).
 */
function decodeBase64ToBytes(base64: string): Uint8Array {
	const binary = atob(base64);
	const bytes = new Uint8Array(binary.length);
	for (let i = 0; i < binary.length; i++) {
		bytes[i] = binary.charCodeAt(i);
	}
	return bytes;
}

/**
 * Encode raw bytes to a base64 string for transport to the native side.
 */
function encodeBytesToBase64(content: Uint8Array): string {
	// Build a binary string in chunks to avoid exceeding the argument limit of
	// String.fromCharCode for large files.
	let binary = '';
	const chunkSize = 0x8000;
	for (let i = 0; i < content.length; i += chunkSize) {
		const chunk = content.subarray(i, i + chunkSize);
		binary += String.fromCharCode.apply(null, chunk as unknown as number[]);
	}
	return btoa(binary);
}
