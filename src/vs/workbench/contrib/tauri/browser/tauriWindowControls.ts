/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import './media/tauriTitlebar.css';
import { $, addDisposableListener, append, EventType } from '../../../../base/browser/dom.js';
import { mainWindow } from '../../../../base/browser/window.js';
import { timeout } from '../../../../base/common/async.js';
import { Codicon } from '../../../../base/common/codicons.js';
import { Disposable } from '../../../../base/common/lifecycle.js';
import { ThemeIcon } from '../../../../base/common/themables.js';
import { Extensions as ConfigurationExtensions, IConfigurationRegistry } from '../../../../platform/configuration/common/configurationRegistry.js';
import { Registry } from '../../../../platform/registry/common/platform.js';
import { isTauri, tauriInvoke } from '../../../../platform/tauri/common/tauri.js';
import { IWorkbenchContribution } from '../../../common/contributions.js';
import { IWorkbenchLayoutService, Parts } from '../../../services/layout/browser/layoutService.js';

/**
 * code-rs title bar:
 *  - the full menu bar (File, Edit, Selection, …) is shown in the title bar by default, with
 *    the overflow "…" menu when it does not fit, like VS Code desktop (web defaults to the
 *    compact hamburger menu);
 *  - on Windows the window has no native frame, so the minimize / maximize / close controls,
 *    dragging and double-click-to-maximize are provided here, with the same markup and
 *    styles as the desktop build.
 */

if (isTauri()) {
	Registry.as<IConfigurationRegistry>(ConfigurationExtensions.Configuration).registerDefaultConfigurations([{
		overrides: { 'window.menuBarVisibility': 'classic' },
	}]);
}

/** Elements inside the title bar that must keep receiving clicks (not start a window drag). */
const INTERACTIVE = '.window-controls-container, .menubar, .action-item, .monaco-action-bar, .command-center, .titlebar-center .monaco-toolbar, input, button, a, [role="button"]';

export class TauriWindowControlsContribution extends Disposable implements IWorkbenchContribution {

	static readonly ID = 'workbench.contrib.tauriWindowControls';

	private maxRestore: HTMLElement | undefined;

	constructor(@IWorkbenchLayoutService private readonly layoutService: IWorkbenchLayoutService) {
		super();
		if (!isTauri()) {
			return;
		}
		this.install();
	}

	private async install(): Promise<void> {
		if (!await tauriInvoke<boolean>('window_has_custom_frame')) {
			return;
		}
		// The title bar may not be rendered yet (slow starts, e.g. the unbundled dev build):
		// wait for it instead of giving up, or the window would have no controls at all.
		let titlebar: HTMLElement | undefined;
		let container: HTMLElement | null | undefined;
		for (let attempt = 0; attempt < 300 && !this._store.isDisposed; attempt++) {
			titlebar = this.layoutService.getContainer(mainWindow, Parts.TITLEBAR_PART);
			// eslint-disable-next-line no-restricted-syntax
			container = titlebar?.querySelector<HTMLElement>('.titlebar-right .window-controls-container');
			if (titlebar && container) {
				break;
			}
			await timeout(50);
		}
		if (!titlebar || !container) {
			return;
		}
		this.layoutService.mainContainer.classList.add('tauri-custom-frame');

		const minimize = append(container, $('div.window-icon.window-minimize' + ThemeIcon.asCSSSelector(Codicon.chromeMinimize)));
		this._register(addDisposableListener(minimize, EventType.CLICK, () => tauriInvoke('window_minimize')));

		this.maxRestore = append(container, $('div.window-icon.window-max-restore'));
		this._register(addDisposableListener(this.maxRestore, EventType.CLICK, () => this.toggleMaximize()));

		const close = append(container, $('div.window-icon.window-close' + ThemeIcon.asCSSSelector(Codicon.chromeClose)));
		this._register(addDisposableListener(close, EventType.CLICK, () => tauriInvoke('window_close')));

		// Drag the window from empty title bar space; double click maximizes/restores.
		this._register(addDisposableListener(titlebar, EventType.MOUSE_DOWN, e => {
			if (e.button === 0 && e.detail === 1 && !(e.target as HTMLElement).closest(INTERACTIVE)) {
				tauriInvoke('window_start_drag');
			}
		}));
		this._register(addDisposableListener(titlebar, EventType.DBLCLICK, e => {
			if (!(e.target as HTMLElement).closest(INTERACTIVE)) {
				this.toggleMaximize();
			}
		}));

		this._register(addDisposableListener(mainWindow, EventType.RESIZE, () => this.refreshMaximized()));
		this.refreshMaximized();
	}

	private async toggleMaximize(): Promise<void> {
		this.render(await tauriInvoke<boolean>('window_toggle_maximize'));
	}

	private async refreshMaximized(): Promise<void> {
		this.render(await tauriInvoke<boolean>('window_is_maximized'));
	}

	private render(maximized: boolean): void {
		if (!this.maxRestore) {
			return;
		}
		this.maxRestore.classList.remove(...ThemeIcon.asClassNameArray(maximized ? Codicon.chromeMaximize : Codicon.chromeRestore));
		this.maxRestore.classList.add(...ThemeIcon.asClassNameArray(maximized ? Codicon.chromeRestore : Codicon.chromeMaximize));
	}
}
