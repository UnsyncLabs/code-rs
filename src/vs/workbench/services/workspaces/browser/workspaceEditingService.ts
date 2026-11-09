/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import { IWorkspaceContextService, toWorkspaceIdentifier } from '../../../../platform/workspace/common/workspace.js';
import { IJSONEditingService } from '../../configuration/common/jsonEditing.js';
import { IWorkspacesService } from '../../../../platform/workspaces/common/workspaces.js';
import { WorkspaceService } from '../../configuration/browser/configurationService.js';
import { ICommandService } from '../../../../platform/commands/common/commands.js';
import { INotificationService } from '../../../../platform/notification/common/notification.js';
import { IFileService } from '../../../../platform/files/common/files.js';
import { IWorkbenchEnvironmentService } from '../../environment/common/environmentService.js';
import { IFileDialogService, IDialogService } from '../../../../platform/dialogs/common/dialogs.js';
import { ITextFileService } from '../../textfile/common/textfiles.js';
import { IHostService } from '../../host/browser/host.js';
import { AbstractWorkspaceEditingService } from './abstractWorkspaceEditingService.js';
import { IWorkspaceEditingService } from '../common/workspaceEditing.js';
import { InstantiationType, registerSingleton } from '../../../../platform/instantiation/common/extensions.js';
import { URI } from '../../../../base/common/uri.js';
import { IUriIdentityService } from '../../../../platform/uriIdentity/common/uriIdentity.js';
import { IWorkspaceTrustManagementService } from '../../../../platform/workspace/common/workspaceTrust.js';
import { IWorkbenchConfigurationService } from '../../configuration/common/configuration.js';
import { IUserDataProfilesService } from '../../../../platform/userDataProfile/common/userDataProfile.js';
import { IUserDataProfileService } from '../../userDataProfile/common/userDataProfile.js';
import { ILogService } from '../../../../platform/log/common/log.js';
import { IExtensionService } from '../../extensions/common/extensions.js';
import { IStorageService } from '../../../../platform/storage/common/storage.js';
import { isTauri } from '../../../../platform/tauri/common/tauri.js';
import { localize } from '../../../../nls.js';

export class BrowserWorkspaceEditingService extends AbstractWorkspaceEditingService {

	constructor(
		@IJSONEditingService jsonEditingService: IJSONEditingService,
		@IWorkspaceContextService contextService: WorkspaceService,
		@IWorkbenchConfigurationService configurationService: IWorkbenchConfigurationService,
		@INotificationService notificationService: INotificationService,
		@ICommandService commandService: ICommandService,
		@IFileService fileService: IFileService,
		@ITextFileService textFileService: ITextFileService,
		@IWorkspacesService workspacesService: IWorkspacesService,
		@IWorkbenchEnvironmentService environmentService: IWorkbenchEnvironmentService,
		@IFileDialogService fileDialogService: IFileDialogService,
		@IDialogService dialogService: IDialogService,
		@IHostService hostService: IHostService,
		@IUriIdentityService uriIdentityService: IUriIdentityService,
		@IWorkspaceTrustManagementService workspaceTrustManagementService: IWorkspaceTrustManagementService,
		@IUserDataProfilesService userDataProfilesService: IUserDataProfilesService,
		@IUserDataProfileService userDataProfileService: IUserDataProfileService,
		@ILogService logService: ILogService,
		@IExtensionService private readonly extensionService: IExtensionService,
		@IStorageService private readonly storageService: IStorageService,
	) {
		super(jsonEditingService, contextService, configurationService, notificationService, commandService, fileService, textFileService, workspacesService, environmentService, fileDialogService, dialogService, hostService, uriIdentityService, workspaceTrustManagementService, userDataProfilesService, userDataProfileService, logService);
	}

	async enterWorkspace(workspaceUri: URI): Promise<void> {
		if (isTauri()) {
			return this.enterWorkspaceInPlace(workspaceUri);
		}

		const oldWorkspace = toWorkspaceIdentifier(this.contextService.getWorkspace());
		const result = await this.doEnterWorkspace(workspaceUri);
		if (result) {

			// Fire event to allow participants to join
			// and possibly migrate data into this workspace
			await this.fireDidEnterWorkspace(oldWorkspace, result.workspace);

			// Open workspace in same window
			await this.hostService.openWindow([{ workspaceUri }], { forceReuseWindow: true });
		}
	}

	/**
	 * code-rs (Tauri): enter a multi-root workspace without reloading the window,
	 * the way the desktop build does (see electron-browser/workspaceEditingService.ts):
	 * stop the extension hosts, switch the workspace and its storage in place,
	 * then restart the extension hosts.
	 */
	private async enterWorkspaceInPlace(workspaceUri: URI): Promise<void> {
		const stopped = await this.extensionService.stopExtensionHosts(localize('restartExtensionHost.reason', "Opening a multi-root workspace"));
		if (!stopped) {
			return;
		}

		try {
			const oldWorkspace = toWorkspaceIdentifier(this.contextService.getWorkspace());
			const result = await this.doEnterWorkspace(workspaceUri);
			if (result) {
				// Migrate storage to the new workspace
				await this.storageService.switch(result.workspace, true /* preserve data */);

				// Fire event to allow participants to join
				await this.fireDidEnterWorkspace(oldWorkspace, result.workspace);
			}
		} catch (error) {
			try {
				await this.extensionService.startExtensionHosts();
			} catch (restartError) {
				this.logService.error(restartError);
			}
			throw error;
		}

		// Entering a workspace means a new storage location and a possibly
		// different `workspace.rootPath`, so restart the extension hosts.
		this.extensionService.startExtensionHosts();
	}
}

registerSingleton(IWorkspaceEditingService, BrowserWorkspaceEditingService, InstantiationType.Delayed);
