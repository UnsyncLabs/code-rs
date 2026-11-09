/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import { Schemas } from '../../../../base/common/network.js';
import { Registry } from '../../../../platform/registry/common/platform.js';
import { IInstantiationService } from '../../../../platform/instantiation/common/instantiation.js';
import { ILogService } from '../../../../platform/log/common/log.js';
import {
	ITerminalBackendRegistry,
	TerminalExtensions,
} from '../../../../platform/terminal/common/terminal.js';
import { isTauri } from '../../../../platform/tauri/common/tauri.js';
import { IWorkbenchContribution, registerWorkbenchContribution2, WorkbenchPhase } from '../../../common/contributions.js';
import { ISearchService, SearchProviderType } from '../../../services/search/common/search.js';
import { TauriSearchProvider } from '../../../services/search/browser/tauriSearchProvider.js';
import { ITerminalInstanceService } from '../../terminal/browser/terminal.js';
import { TauriTerminalBackend } from './tauriTerminalBackend.js';
import { TauriThemeBackgroundContribution } from './tauriThemeBackground.js';
import { TauriUpdatesContribution } from './tauriUpdates.js';
import { TauriMcpContribution } from './tauriMcp.js';
import { TauriChatAgentContribution } from './tauriChatAgent.js';
import { TauriExtensionAuditContribution } from './tauriExtensionAudit.js';
import { TauriWindowControlsContribution } from './tauriWindowControls.js';
import './tauriAssistant.js';
import { TauriChatModelsContribution } from './tauriChatModels.js';

/**
 * Wires the native Rust/Tauri services into the web workbench when it is hosted
 * by the Tauri shell (code-rs). Outside Tauri every registration is skipped and
 * the workbench keeps its normal web behaviour.
 *
 * Registered here:
 *  - {@link TauriTerminalBackend} as the local terminal backend
 *    (`remoteAuthority === undefined`).
 *  - {@link TauriSearchProvider} for file + text search on the `file` scheme.
 *
 * Wired elsewhere (kept close to where that subsystem is composed):
 *  - The `file` scheme `TauriFileSystemProvider` is registered in
 *    `browser/web.main.ts` (where the browser `file` provider would otherwise
 *    be installed, so there is no double-registration).
 *  - The local-process extension host is created by `TauriExtensionHostFactory`
 *    inside `services/extensions/browser/extensionService.ts` (where the host
 *    factory is constructed).
 *
 * See docs/rust-rewrite/WIRING.md.
 */
class TauriWorkbenchContribution implements IWorkbenchContribution {

	static readonly ID = 'workbench.contrib.tauri';

	constructor(
		@IInstantiationService instantiationService: IInstantiationService,
		@ISearchService searchService: ISearchService,
		@ITerminalInstanceService terminalInstanceService: ITerminalInstanceService,
		@ILogService logService: ILogService,
	) {
		if (!isTauri()) {
			return; // not inside the Tauri shell — fall back to web behaviour
		}

		logService.info('[tauri] Detected Tauri shell; wiring native terminal + search services.');

		// --- Terminal backend (local, remoteAuthority === undefined) ------
		const backend = instantiationService.createInstance(TauriTerminalBackend);
		Registry.as<ITerminalBackendRegistry>(TerminalExtensions.Backend).registerTerminalBackend(backend);
		terminalInstanceService.didRegisterBackend(backend);

		// --- Search provider (file + text on the file scheme) -------------
		const searchProvider = instantiationService.createInstance(TauriSearchProvider);
		searchService.registerSearchResultProvider(Schemas.file, SearchProviderType.file, searchProvider);
		searchService.registerSearchResultProvider(Schemas.file, SearchProviderType.text, searchProvider);
	}
}

// Register early (BlockStartup) so the terminal backend is present before the
// terminal service resolves a backend for the local authority, mirroring the
// Electron `LocalTerminalBackendContribution` registration.
registerWorkbenchContribution2(TauriWorkbenchContribution.ID, TauriWorkbenchContribution, WorkbenchPhase.BlockStartup);

registerWorkbenchContribution2(TauriThemeBackgroundContribution.ID, TauriThemeBackgroundContribution, WorkbenchPhase.AfterRestored);
registerWorkbenchContribution2(TauriUpdatesContribution.ID, TauriUpdatesContribution, WorkbenchPhase.Eventually);
registerWorkbenchContribution2(TauriMcpContribution.ID, TauriMcpContribution, WorkbenchPhase.AfterRestored);
registerWorkbenchContribution2(TauriExtensionAuditContribution.ID, TauriExtensionAuditContribution, WorkbenchPhase.AfterRestored);
registerWorkbenchContribution2(TauriWindowControlsContribution.ID, TauriWindowControlsContribution, WorkbenchPhase.BlockRestore);
registerWorkbenchContribution2(TauriChatAgentContribution.ID, TauriChatAgentContribution, WorkbenchPhase.AfterRestored);
registerWorkbenchContribution2(TauriChatModelsContribution.ID, TauriChatModelsContribution, WorkbenchPhase.AfterRestored);
