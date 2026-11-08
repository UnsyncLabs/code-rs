/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import { CancellationToken } from '../../../../base/common/cancellation.js';
import { Disposable, toDisposable } from '../../../../base/common/lifecycle.js';
import { URI } from '../../../../base/common/uri.js';
import { generateUuid } from '../../../../base/common/uuid.js';
import {
	IFileMatch,
	IFileQuery,
	ISearchComplete,
	ISearchProgressItem,
	ISearchResultProvider,
	ITextQuery,
	ITextSearchMatch,
	resolveExcludePatternsForProvider,
	resolvePatternsForProvider,
} from '../common/search.js';

/**
 * Minimal view of the Tauri 2 internals bridge that the Rust shell injects on
 * `window`. We depend only on `invoke` and `transformCallback`; event
 * subscription is implemented on top of them exactly like `@tauri-apps/api`'s
 * `event.listen` does (`plugin:event|listen` / `plugin:event|unlisten`).
 *
 * See `src-tauri/src/services/search.rs` for the Rust counterpart.
 */
interface TauriInternals {
	invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T>;
	transformCallback<T>(callback: (payload: T) => void, once?: boolean): number;
}

interface TauriEventPluginInternals {
	unregisterListener(event: string, eventId: number): void;
}

declare global {
	interface Window {
		__TAURI_INTERNALS__?: TauriInternals;
		__TAURI_EVENT_PLUGIN_INTERNALS__?: TauriEventPluginInternals;
	}
}

/** Shape of a Tauri event payload envelope delivered to listener callbacks. */
interface TauriEvent<T> {
	event: string;
	id: number;
	payload: T;
}

type UnlistenFn = () => void;

// --- Wire shapes mirroring the Rust service (search.rs) ---------------------

interface RustSearchRange {
	startLineNumber: number;
	startColumn: number;
	endLineNumber: number;
	endColumn: number;
}

interface RustRangeSetPairing {
	source: RustSearchRange;
	preview: RustSearchRange;
}

interface RustTextSearchMatch {
	rangeLocations: RustRangeSetPairing[];
	previewText: string;
}

/** Payload of `search://result/{id}` — mirrors `FileMatchEvent` in Rust. */
interface RustFileMatchEvent {
	id: string;
	path: string;
	results: RustTextSearchMatch[];
}

/** Payload of `search://done/{id}` — mirrors `SearchDoneEvent` in Rust. */
interface RustSearchDoneEvent {
	id: string;
	limitHit: boolean;
	error?: string;
}

/**
 * {@link ISearchResultProvider} implementation that delegates file and text
 * search to the Rust/Tauri `search_text` / `search_files` commands and consumes
 * their streamed `search://result/{id}` and `search://done/{id}` events.
 *
 * Register it in the workbench the same way the Electron/web providers are
 * registered, e.g.:
 *
 * ```ts
 * const provider = instantiationService.createInstance(TauriSearchProvider);
 * searchService.registerSearchResultProvider(Schemas.file, SearchProviderType.text, provider);
 * searchService.registerSearchResultProvider(Schemas.file, SearchProviderType.file, provider);
 * ```
 */
export class TauriSearchProvider extends Disposable implements ISearchResultProvider {

	async getAIName(): Promise<string | undefined> {
		return undefined;
	}

	async textSearch(
		query: ITextQuery,
		onProgress?: (p: ISearchProgressItem) => void,
		token?: CancellationToken
	): Promise<ISearchComplete> {
		const internals = requireInternals();
		const id = generateUuid();

		const folders = query.folderQueries.map(fq => fq.folder.fsPath);
		const includes = resolvePatternsForProvider(query.includePattern, undefined);
		const excludes = resolveExcludePatternsForProvider(query.excludePattern, undefined);

		const results: IFileMatch[] = [];

		const rustQuery = {
			id,
			pattern: query.contentPattern.pattern,
			folders,
			isRegExp: !!query.contentPattern.isRegExp,
			isCaseSensitive: !!query.contentPattern.isCaseSensitive,
			isWordMatch: !!query.contentPattern.isWordMatch,
			includes,
			excludes,
			maxResults: query.maxResults,
			previewContextLines: query.surroundingContext ?? 0,
			useIgnoreFiles: !query.userDisabledExcludesAndIgnoreFiles,
		};

		return this.runStreamingSearch(
			internals,
			id,
			'search_text',
			rustQuery,
			token,
			results,
			(event) => {
				const match = toFileMatch(event);
				results.push(match);
				onProgress?.(match);
			}
		);
	}

	async fileSearch(query: IFileQuery, token?: CancellationToken): Promise<ISearchComplete> {
		const internals = requireInternals();
		const id = generateUuid();

		const folders = query.folderQueries.map(fq => fq.folder.fsPath);
		const includes = resolvePatternsForProvider(query.includePattern, undefined);
		const excludes = resolveExcludePatternsForProvider(query.excludePattern, undefined);

		const results: IFileMatch[] = [];

		const rustQuery = {
			id,
			pattern: query.filePattern ?? '',
			folders,
			includes,
			excludes,
			maxResults: query.maxResults,
			useIgnoreFiles: !query.userDisabledExcludesAndIgnoreFiles,
		};

		return this.runStreamingSearch(
			internals,
			id,
			'search_files',
			rustQuery,
			token,
			results,
			(event) => {
				// File-name results carry no text matches.
				results.push({ resource: URI.file(event.path) });
			}
		);
	}

	async clearCache(_cacheKey: string): Promise<void> {
		// The Rust engine does not cache results; nothing to do.
	}

	/**
	 * Shared driver: subscribes to the result + done events, invokes the Rust
	 * command, wires cancellation to `search_cancel`, and resolves once the
	 * `search://done/{id}` event arrives.
	 */
	private async runStreamingSearch(
		internals: TauriInternals,
		id: string,
		command: 'search_text' | 'search_files',
		rustQuery: Record<string, unknown>,
		token: CancellationToken | undefined,
		results: IFileMatch[],
		onResult: (event: RustFileMatchEvent) => void
	): Promise<ISearchComplete> {
		const disposables = this._register(new DisposableTracker());

		return new Promise<ISearchComplete>((resolve, reject) => {
			let settled = false;
			const finish = (complete: ISearchComplete) => {
				if (settled) {
					return;
				}
				settled = true;
				disposables.dispose();
				resolve(complete);
			};
			const fail = (err: unknown) => {
				if (settled) {
					return;
				}
				settled = true;
				disposables.dispose();
				reject(err instanceof Error ? err : new Error(String(err)));
			};

			// Subscribe BEFORE invoking so no early results are missed.
			Promise.all([
				listen<RustFileMatchEvent>(internals, `search://result/${id}`, (ev) => {
					if (ev.payload.id === id) {
						onResult(ev.payload);
					}
				}),
				listen<RustSearchDoneEvent>(internals, `search://done/${id}`, (ev) => {
					if (ev.payload.id !== id) {
						return;
					}
					if (ev.payload.error) {
						fail(new Error(ev.payload.error));
						return;
					}
					finish({
						results,
						limitHit: !!ev.payload.limitHit,
						messages: [],
					});
				}),
			]).then(([unlistenResult, unlistenDone]) => {
				disposables.add(toDisposable(unlistenResult));
				disposables.add(toDisposable(unlistenDone));

				// Cancellation: ask Rust to stop; it will still emit done.
				if (token) {
					disposables.add(token.onCancellationRequested(() => {
						void internals.invoke('search_cancel', { id }).catch(() => { /* best-effort */ });
					}));
					if (token.isCancellationRequested) {
						void internals.invoke('search_cancel', { id }).catch(() => { /* best-effort */ });
					}
				}

				// Kick off the search. The command resolves immediately; results
				// arrive via events. A rejection here is a hard failure.
				internals.invoke(command, { query: rustQuery }).catch(fail);
			}).catch(fail);
		});
	}
}

/** Tracks and disposes a set of disposables; small local helper. */
class DisposableTracker extends Disposable {
	add<T extends { dispose(): void }>(d: T): T {
		return this._register(d);
	}
}

// ---------------------------------------------------------------------------
// Tauri event helpers (reimplementation of @tauri-apps/api event.listen using
// only window.__TAURI_INTERNALS__, as mandated by the task).
// ---------------------------------------------------------------------------

function requireInternals(): TauriInternals {
	const internals = window.__TAURI_INTERNALS__;
	if (!internals) {
		throw new Error('TauriSearchProvider: window.__TAURI_INTERNALS__ is not available (not running inside the Tauri shell)');
	}
	return internals;
}

/**
 * Subscribes to a Tauri event, returning a promise that resolves to an
 * unlisten function. Mirrors `@tauri-apps/api`'s `listen`:
 * `invoke('plugin:event|listen', { event, target, handler })`.
 */
async function listen<T>(
	internals: TauriInternals,
	event: string,
	handler: (ev: TauriEvent<T>) => void
): Promise<UnlistenFn> {
	const callbackId = internals.transformCallback<TauriEvent<T>>((payload) => handler(payload));
	const eventId = await internals.invoke<number>('plugin:event|listen', {
		event,
		target: { kind: 'Any' },
		handler: callbackId,
	});

	return () => {
		// Best-effort synchronous local unregister + async backend unlisten,
		// matching the reference implementation's `_unlisten`.
		window.__TAURI_EVENT_PLUGIN_INTERNALS__?.unregisterListener(event, eventId);
		void internals.invoke('plugin:event|unlisten', { event, eventId }).catch(() => { /* ignore */ });
	};
}

/** Converts a Rust `FileMatchEvent` into VS Code's {@link IFileMatch}. */
function toFileMatch(event: RustFileMatchEvent): IFileMatch {
	const resource = URI.file(event.path);
	const results: ITextSearchMatch[] = event.results.map(r => ({
		previewText: r.previewText,
		rangeLocations: r.rangeLocations.map(loc => ({
			source: {
				startLineNumber: loc.source.startLineNumber,
				startColumn: loc.source.startColumn,
				endLineNumber: loc.source.endLineNumber,
				endColumn: loc.source.endColumn,
			},
			preview: {
				startLineNumber: loc.preview.startLineNumber,
				startColumn: loc.preview.startColumn,
				endLineNumber: loc.preview.endLineNumber,
				endColumn: loc.preview.endColumn,
			},
		})),
	}));
	return { resource, results };
}
