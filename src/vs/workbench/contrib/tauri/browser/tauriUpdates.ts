/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import { Disposable } from '../../../../base/common/lifecycle.js';
import Severity from '../../../../base/common/severity.js';
import { localize, localize2 } from '../../../../nls.js';
import { Action2, registerAction2 } from '../../../../platform/actions/common/actions.js';
import { ConfigurationScope, Extensions as ConfigurationExtensions, IConfigurationRegistry } from '../../../../platform/configuration/common/configurationRegistry.js';
import { IConfigurationService } from '../../../../platform/configuration/common/configuration.js';
import { IDialogService } from '../../../../platform/dialogs/common/dialogs.js';
import { ServicesAccessor } from '../../../../platform/instantiation/common/instantiation.js';
import { INotificationService } from '../../../../platform/notification/common/notification.js';
import { Registry } from '../../../../platform/registry/common/platform.js';
import { IStorageService, StorageScope, StorageTarget } from '../../../../platform/storage/common/storage.js';
import { isTauri, tauriInvoke } from '../../../../platform/tauri/common/tauri.js';
import { IWorkbenchContribution } from '../../../common/contributions.js';

/**
 * code-rs updates:
 *  - `codeRs.updates.enabled` (on by default) checks for a new release at startup.
 *  - The first launch asks once whether to turn updates off, warning that it is not recommended.
 *  - "Check for Updates" (also a link in the setting) downloads, verifies and installs the
 *    update for installer builds (Windows, macOS, AppImage); package-manager builds
 *    (deb, rpm, pacman) and local builds open the releases page instead.
 */

const UPDATES_ENABLED_SETTING = 'codeRs.updates.enabled';
const FIRST_RUN_PROMPT_KEY = 'codeRs.updates.firstRunPromptShown';
const CHECK_FOR_UPDATES_COMMAND = 'codeRs.checkForUpdates';

interface IUpdaterInfo {
	readonly currentVersion: string;
	readonly installKind: 'installer' | 'package-manager' | 'dev';
	readonly canSelfUpdate: boolean;
	readonly releasesUrl: string;
}

interface IAvailableUpdate {
	readonly version: string;
	readonly notes?: string;
}

if (isTauri()) {
	Registry.as<IConfigurationRegistry>(ConfigurationExtensions.Configuration).registerConfiguration({
		id: 'codeRsUpdates',
		order: 15,
		title: localize('codeRsUpdatesTitle', "Updates"),
		type: 'object',
		properties: {
			[UPDATES_ENABLED_SETTING]: {
				type: 'boolean',
				default: true,
				scope: ConfigurationScope.APPLICATION,
				tags: ['usesOnlineServices'],
				markdownDescription: localize('codeRsUpdatesEnabled', "Check for new code-rs releases when the app starts. Turning this off is not recommended: updates include security fixes.\n\n[Check for updates and install](command:{0})", CHECK_FOR_UPDATES_COMMAND),
			}
		}
	});
}

async function installOrOpen(info: IUpdaterInfo, update: IAvailableUpdate, notificationService: INotificationService): Promise<void> {
	if (info.canSelfUpdate) {
		const handle = notificationService.notify({ severity: Severity.Info, message: localize('codeRsDownloading', "Downloading code-rs {0}…", update.version), progress: { infinite: true } });
		try {
			await tauriInvoke('updater_install'); // restarts the app on success
		} catch (error) {
			handle.close();
			notificationService.error(localize('codeRsUpdateFailed', "Could not install the update: {0}", String(error)));
		}
	} else {
		await tauriInvoke('updater_open_releases');
	}
}

async function checkForUpdates(notificationService: INotificationService, dialogService: IDialogService, interactive: boolean): Promise<void> {
	const info = await tauriInvoke<IUpdaterInfo>('updater_info');
	let update: IAvailableUpdate | null;
	try {
		update = await tauriInvoke<IAvailableUpdate | null>('updater_check');
	} catch (error) {
		if (interactive) {
			notificationService.error(localize('codeRsCheckFailed', "Could not check for updates: {0}", String(error)));
		}
		return;
	}

	if (!update) {
		if (interactive) {
			notificationService.info(localize('codeRsUpToDate', "code-rs {0} is up to date.", info.currentVersion));
		}
		return;
	}

	const installLabel = info.canSelfUpdate ? localize('codeRsInstallRestart', "Install and Restart") : localize('codeRsOpenDownloads', "Open Download Page");
	const detail = info.canSelfUpdate
		? undefined
		: localize('codeRsManagedInstall', "This installation is managed by your package manager (or is a local build), so the new version has to be downloaded from the releases page.");

	if (interactive) {
		const { confirmed } = await dialogService.confirm({
			type: Severity.Info,
			message: localize('codeRsUpdateAvailable', "code-rs {0} is available (you have {1}).", update.version, info.currentVersion),
			detail,
			primaryButton: installLabel,
		});
		if (confirmed) {
			await installOrOpen(info, update, notificationService);
		}
		return;
	}

	notificationService.prompt(Severity.Info, localize('codeRsUpdateAvailable', "code-rs {0} is available (you have {1}).", update.version, info.currentVersion), [
		{ label: installLabel, run: () => installOrOpen(info, update, notificationService) },
	]);
}

export class TauriUpdatesContribution extends Disposable implements IWorkbenchContribution {

	static readonly ID = 'workbench.contrib.tauriUpdates';

	constructor(
		@IConfigurationService private readonly configurationService: IConfigurationService,
		@IStorageService private readonly storageService: IStorageService,
		@IDialogService private readonly dialogService: IDialogService,
		@INotificationService private readonly notificationService: INotificationService,
	) {
		super();
		if (!isTauri()) {
			return;
		}
		this.run();
	}

	private async run(): Promise<void> {
		// One-time question on first launch: updates stay on unless the user opts out.
		if (!this.storageService.getBoolean(FIRST_RUN_PROMPT_KEY, StorageScope.APPLICATION, false)) {
			this.storageService.store(FIRST_RUN_PROMPT_KEY, true, StorageScope.APPLICATION, StorageTarget.MACHINE);
			const { result } = await this.dialogService.prompt<boolean>({
				type: Severity.Warning,
				message: localize('codeRsDisableUpdatesQuestion', "Do you want to disable automatic updates?"),
				detail: localize('codeRsDisableUpdatesDetail', "This is not recommended: updates include security fixes. You will only be asked once; you can change it later in Settings > Updates, where you can also check for and install updates manually."),
				buttons: [
					{ label: localize('codeRsKeepUpdates', "Keep Updates On (Recommended)"), run: () => false },
					{ label: localize('codeRsDisableUpdates', "Disable Updates"), run: () => true },
				],
				cancelButton: { label: localize('codeRsKeepUpdatesCancel', "Keep Updates On"), run: () => false },
			});
			if (result) {
				await this.configurationService.updateValue(UPDATES_ENABLED_SETTING, false);
			}
		}

		if (this.configurationService.getValue<boolean>(UPDATES_ENABLED_SETTING) !== false) {
			await checkForUpdates(this.notificationService, this.dialogService, false);
		}
	}
}

registerAction2(class extends Action2 {
	constructor() {
		super({
			id: CHECK_FOR_UPDATES_COMMAND,
			title: localize2('codeRsCheckForUpdates', "Check for Updates and Install"),
			category: localize2('codeRsCategory', "code-rs"),
			f1: true,
		});
	}

	run(accessor: ServicesAccessor): Promise<void> {
		return checkForUpdates(accessor.get(INotificationService), accessor.get(IDialogService), true);
	}
});
