/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

/**
 * Detection of the Tauri (code-rs) shell from the web workbench.
 *
 * The Rust shell injects `window.__TAURI_INTERNALS__` into the webview (always,
 * independent of `app.withGlobalTauri`). The four TS bridges
 * (`tauriFileSystemProvider`, `tauriPty`, `tauriSearchProvider`,
 * `tauriExtensionHost`) all probe this same global, so it is the single source
 * of truth for "are we running inside Tauri?".
 *
 * This lives in `platform/.../common` (rather than under a workbench contrib)
 * so both the core `browser/web.main.ts` bootstrap and workbench contributions
 * can depend on it without crossing layering boundaries.
 *
 * See docs/rust-rewrite/TAURI_SETUP.md and docs/rust-rewrite/EXTHOST.md.
 */

interface ITauriDetectionWindow {
	/** Injected by the Tauri runtime; carries the `invoke` bridge. */
	__TAURI_INTERNALS__?: unknown;
	/** Present only when `app.withGlobalTauri: true` (used by the ext host). */
	__TAURI__?: unknown;
}

/**
 * Returns `true` when the workbench is hosted by the Tauri shell.
 *
 * This is the canonical check used to decide whether to wire the native Rust
 * services instead of the default web behaviour. It is intentionally cheap and
 * synchronous so it can gate contribution registration at startup.
 */
export function isTauri(): boolean {
	return typeof globalThis !== 'undefined'
		&& !!(globalThis as unknown as ITauriDetectionWindow).__TAURI_INTERNALS__;
}

/**
 * Returns `true` when the public `window.__TAURI__` global is available.
 *
 * The extension host bridge (`tauriExtensionHost.ts`) needs this richer global
 * (`core.invoke` + `event.listen`), which only exists when
 * `app.withGlobalTauri: true` is set in `src-tauri/tauri.conf.json`. The file
 * system / pty / search bridges work off `__TAURI_INTERNALS__` alone and do not
 * need this.
 */
export function hasTauriGlobalApi(): boolean {
	return typeof globalThis !== 'undefined'
		&& !!(globalThis as unknown as ITauriDetectionWindow).__TAURI__;
}

/**
 * The URL query parameter the Tauri shell uses to pass the folder to open as an
 * absolute filesystem path. `workbench.ts` turns this into a `file://` URI and
 * uses it as the default workspace when no explicit `?folder=`/`?workspace=` is
 * present. Example: `index.html?tauriFolder=/home/me/project`.
 */
export const TAURI_FOLDER_QUERY_PARAM = 'tauriFolder';

interface ITauriInvokeWindow {
	__TAURI_INTERNALS__?: { invoke(cmd: string, args?: unknown): Promise<unknown> };
}

export interface ITauriDialogFilter {
	readonly name: string;
	readonly extensions: string[];
}

export interface ITauriOpenDialogOptions {
	readonly title?: string;
	readonly defaultPath?: string;
	readonly directory?: boolean;
	readonly multiple?: boolean;
	readonly filters?: ITauriDialogFilter[];
}

export function tauriInvoke<T>(cmd: string, args?: unknown): Promise<T> {
	const internals = (globalThis as unknown as ITauriInvokeWindow).__TAURI_INTERNALS__;
	if (!internals) {
		return Promise.reject(new Error('Tauri runtime not available'));
	}
	return internals.invoke(cmd, args) as Promise<T>;
}

/**
 * Native open dialog (tauri-plugin-dialog). Resolves to absolute filesystem
 * paths, or `undefined` when the user cancels.
 */
export async function tauriShowOpenDialog(options: ITauriOpenDialogOptions): Promise<string[] | undefined> {
	const result = await tauriInvoke<string | string[] | null>('plugin:dialog|open', { options });
	if (!result) {
		return undefined;
	}
	return Array.isArray(result) ? result : [result];
}

/**
 * Native save dialog (tauri-plugin-dialog). Resolves to an absolute filesystem
 * path, or `undefined` when the user cancels.
 */
export async function tauriShowSaveDialog(options: Omit<ITauriOpenDialogOptions, 'directory' | 'multiple'>): Promise<string | undefined> {
	const result = await tauriInvoke<string | null>('plugin:dialog|save', { options });
	return result ?? undefined;
}
