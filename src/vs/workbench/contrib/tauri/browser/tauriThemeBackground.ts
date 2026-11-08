/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import { Disposable } from '../../../../base/common/lifecycle.js';
import { isTauri } from '../../../../platform/tauri/common/tauri.js';
import { editorBackground } from '../../../../platform/theme/common/colorRegistry.js';
import { IColorTheme, IThemeService } from '../../../../platform/theme/common/themeService.js';
import { IWorkbenchContribution } from '../../../common/contributions.js';

/** localStorage key read by the Tauri initialization script before the page paints. */
export const TAURI_THEME_BACKGROUND_KEY = 'code-rs.themeBackground';

interface ITauriInvokeWindow {
	__TAURI_INTERNALS__?: { invoke(cmd: string, args?: unknown): Promise<unknown> };
}

/**
 * Remembers the current theme's editor background so that reloads (opening a
 * folder, closing a folder, switching workspaces) do not flash white: the
 * native window background and the next page's first paint both use it.
 */
export class TauriThemeBackgroundContribution extends Disposable implements IWorkbenchContribution {

	static readonly ID = 'workbench.contrib.tauriThemeBackground';

	private lastColor: string | undefined;

	constructor(@IThemeService themeService: IThemeService) {
		super();
		if (!isTauri()) {
			return;
		}
		this.update(themeService.getColorTheme());
		// The workbench paints its own background now; drop the boot-time style
		// injected by the Tauri initialization script (not created by this file).
		// eslint-disable-next-line no-restricted-syntax
		globalThis.document?.getElementById('code-rs-boot-background')?.remove();
		this._register(themeService.onDidColorThemeChange(theme => this.update(theme)));
	}

	private update(theme: IColorTheme): void {
		const color = theme.getColor(editorBackground)?.toString();
		if (!color || color === this.lastColor) {
			return;
		}
		this.lastColor = color;
		try {
			globalThis.localStorage?.setItem(TAURI_THEME_BACKGROUND_KEY, color);
		} catch {
			// storage unavailable; the native background below still applies
		}
		const internals = (globalThis as unknown as ITauriInvokeWindow).__TAURI_INTERNALS__;
		internals?.invoke('set_theme_background', { color }).catch(() => { /* best effort */ });
	}
}
