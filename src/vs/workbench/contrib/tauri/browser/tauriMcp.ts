/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import { KeyCode, KeyMod } from '../../../../base/common/keyCodes.js';
import { Disposable, MutableDisposable } from '../../../../base/common/lifecycle.js';
import { isAbsolute } from '../../../../base/common/path.js';
import { URI } from '../../../../base/common/uri.js';
import { localize, localize2 } from '../../../../nls.js';
import { Action2, MenuId, registerAction2 } from '../../../../platform/actions/common/actions.js';
import { Codicon } from '../../../../base/common/codicons.js';
import { IClipboardService } from '../../../../platform/clipboard/common/clipboardService.js';
import { ICommandService } from '../../../../platform/commands/common/commands.js';
import { ConfigurationScope, Extensions as ConfigurationExtensions, IConfigurationRegistry } from '../../../../platform/configuration/common/configurationRegistry.js';
import { IConfigurationService } from '../../../../platform/configuration/common/configuration.js';
import { IFileService } from '../../../../platform/files/common/files.js';
import { IInstantiationService, ServicesAccessor } from '../../../../platform/instantiation/common/instantiation.js';
import { KeybindingWeight } from '../../../../platform/keybinding/common/keybindingsRegistry.js';
import { IMarkerService, MarkerSeverity } from '../../../../platform/markers/common/markers.js';
import { INotificationService, Severity } from '../../../../platform/notification/common/notification.js';
import { IQuickInputService, IQuickPickItem, IQuickPickSeparator } from '../../../../platform/quickinput/common/quickInput.js';
import { Registry } from '../../../../platform/registry/common/platform.js';
import { isTauri, ITauriExtensionAuditReport, tauriInvoke } from '../../../../platform/tauri/common/tauri.js';
import { IUriIdentityService } from '../../../../platform/uriIdentity/common/uriIdentity.js';
import { IWorkspaceContextService } from '../../../../platform/workspace/common/workspace.js';
import { IWorkspaceTrustManagementService } from '../../../../platform/workspace/common/workspaceTrust.js';
import { IWorkbenchContribution } from '../../../common/contributions.js';
import { IEditorService } from '../../../services/editor/common/editorService.js';
import { IHostService } from '../../../services/host/browser/host.js';
import { IStatusbarEntryAccessor, IStatusbarService, StatusbarAlignment } from '../../../services/statusbar/browser/statusbar.js';
import { IWorkspaceEditingService } from '../../../services/workspaces/common/workspaceEditing.js';
import { isCodeEditor } from '../../../../editor/browser/editorBrowser.js';
import { CancellationToken } from '../../../../base/common/cancellation.js';
import { IDialogService } from '../../../../platform/dialogs/common/dialogs.js';
import { IStorageService } from '../../../../platform/storage/common/storage.js';
import { TauriMcpWriteTools, WRITE_TOOLS } from './tauriMcpWriteTools.js';
import { setAgentToolExecutor } from './tauriAgentTools.js';
import { ITextFileService } from '../../../services/textfile/common/textfiles.js';
import { IWorkbenchExtensionManagementService } from '../../../services/extensionManagement/common/extensionManagement.js';
import { listProviders, reviewCode, reviewExtensionReport } from './tauriAssistant.js';
import { IExtensionGalleryService } from '../../../../platform/extensionManagement/common/extensionManagement.js';

/**
 * code-rs MCP server, workbench side (design: docs/rust-rewrite/MCP_DESIGN.md, phase 1).
 *
 * Rust owns the transport, authentication and audit log and forwards tool calls here as
 * `mcp://request` events. This contribution runs them against the workbench services and
 * answers with `mcp_respond`. Read (level 0) and UI (level 1) tools are here; tools that
 * change things (levels 2-3) are in tauriMcpWriteTools.ts and always confirm. Files outside the workspace folders are never accessible, and an untrusted workspace is
 * read-only.
 */

const MCP_ENABLED_SETTING = 'codeRs.mcp.enabled';
const SHOW_PANEL_COMMAND = 'codeRs.mcp.showPanel';
const MAX_FILE_BYTES = 1024 * 1024;
const MAX_DIAGNOSTICS = 500;

/** Tools that change what the user sees or the open workspace (level 1). */
const UI_TOOLS = new Set(['workspace_open_folder', 'workspace_add_folder', 'workspace_close_folder', 'editor_open', 'ui_notify', 'ui_status']);

interface IMcpRequest {
	readonly id: number;
	readonly tool: string;
	readonly args: Record<string, unknown>;
	readonly client?: string;
}

interface IMcpStatus {
	readonly enabled: boolean;
	readonly url?: string;
	readonly bridgeCommand: string;
}

interface IAuditEntry {
	readonly time: number;
	readonly client: string;
	readonly tool: string;
	readonly ok: boolean;
	readonly error?: string;
}

interface ITauriEventGlobal {
	__TAURI__?: { event?: { listen<T>(event: string, handler: (e: { payload: T }) => void): Promise<() => void> } };
}

if (isTauri()) {
	Registry.as<IConfigurationRegistry>(ConfigurationExtensions.Configuration).registerConfiguration({
		id: 'codeRsMcp',
		order: 16,
		title: localize('codeRsMcpTitle', "code-rs MCP"),
		type: 'object',
		properties: {
			[MCP_ENABLED_SETTING]: {
				type: 'boolean',
				default: false,
				scope: ConfigurationScope.APPLICATION,
				markdownDescription: localize('codeRsMcpEnabled', "Let AI tools (Claude Code, Claude Desktop, Kiro…) connect to code-rs through MCP. Off by default. Connections are local only and need a token; every tool call is recorded in a local audit log. Open the MCP panel with `Ctrl+Alt+Shift+M` (`Cmd+Alt+Shift+M` on macOS).\n\n[Open MCP panel](command:{0})", SHOW_PANEL_COMMAND),
			}
		}
	});
}

export class TauriMcpContribution extends Disposable implements IWorkbenchContribution {

	static readonly ID = 'workbench.contrib.tauriMcp';

	private readonly writeTools: TauriMcpWriteTools | undefined;
	private readonly statusEntry = this._register(new MutableDisposable<IStatusbarEntryAccessor & { dispose(): void }>());

	constructor(
		@IConfigurationService private readonly configurationService: IConfigurationService,
		@IWorkspaceContextService private readonly contextService: IWorkspaceContextService,
		@IWorkspaceTrustManagementService private readonly trustService: IWorkspaceTrustManagementService,
		@IUriIdentityService private readonly uriIdentityService: IUriIdentityService,
		@IFileService private readonly fileService: IFileService,
		@IEditorService private readonly editorService: IEditorService,
		@IMarkerService private readonly markerService: IMarkerService,
		@INotificationService private readonly notificationService: INotificationService,
		@IHostService private readonly hostService: IHostService,
		@IWorkspaceEditingService private readonly workspaceEditingService: IWorkspaceEditingService,
		@ICommandService private readonly commandService: ICommandService,
		@IStatusbarService private readonly statusbarService: IStatusbarService,
		@IExtensionGalleryService private readonly galleryService: IExtensionGalleryService,
		@IDialogService private readonly dialogService: IDialogService,
		@IStorageService private readonly storageService: IStorageService,
		@IInstantiationService instantiationService: IInstantiationService,
	) {
		super();
		if (!isTauri()) {
			return;
		}
		this.writeTools = this._register(instantiationService.createInstance(TauriMcpWriteTools));
		setAgentToolExecutor((tool, args, client) => this.execute(tool, args, client, action => { action(); }));
		this._register({ dispose: () => setAgentToolExecutor(undefined) });
		this.listen();
		this.applyEnabled();
		instantiationService.invokeFunction(accessor => this.forwardEvents(accessor.get(ITextFileService), accessor.get(IWorkbenchExtensionManagementService)));
		this._register(this.configurationService.onDidChangeConfiguration(e => {
			if (e.affectsConfiguration(MCP_ENABLED_SETTING)) {
				this.applyEnabled();
			}
		}));
	}

	/** Workbench events for MCP clients (notifications/message on the SSE stream). */
	private forwardEvents(textFileService: ITextFileService, extensionManagementService: IWorkbenchExtensionManagementService): void {
		const emit = (kind: string, data: unknown) => {
			if (this.configurationService.getValue<boolean>(MCP_ENABLED_SETTING) === true) {
				tauriInvoke('mcp_emit_event', { kind, data }).catch(() => { /* MCP off */ });
			}
		};
		this._register(textFileService.files.onDidSave(e => emit('file-saved', { path: this.toPath(e.model.resource) })));
		this._register(extensionManagementService.onDidInstallExtensions(results => {
			for (const r of results) {
				emit(r.error ? 'extension-install-failed' : 'extension-installed', { id: r.identifier.id, version: r.local?.manifest.version ?? null });
			}
		}));
		this._register(extensionManagementService.onDidUninstallExtension(e => emit('extension-uninstalled', { id: e.identifier.id })));
		this._register(this.contextService.onDidChangeWorkspaceFolders(() => emit('workspace-folders-changed', { folders: this.folders().map(f => this.toPath(f.uri)) })));
	}

	private async applyEnabled(): Promise<void> {
		const enabled = this.configurationService.getValue<boolean>(MCP_ENABLED_SETTING) === true;
		try {
			await tauriInvoke('mcp_set_enabled', { enabled });
		} catch (error) {
			this.notificationService.error(localize('codeRsMcpStartFailed', "Could not start the MCP server: {0}", String(error)));
			return;
		}
		if (enabled) {
			const entry = {
				name: localize('codeRsMcpStatusName', "code-rs MCP"),
				text: '$(plug) MCP',
				ariaLabel: localize('codeRsMcpStatusAria', "MCP server is on"),
				tooltip: localize('codeRsMcpStatusTooltip', "MCP server is on: AI tools can connect to code-rs. Click to open the MCP panel (Ctrl+Alt+Shift+M)."),
				command: SHOW_PANEL_COMMAND,
			};
			if (!this.statusEntry.value) {
				this.statusEntry.value = this.statusbarService.addEntry(entry, 'codeRs.mcp', StatusbarAlignment.RIGHT, 100);
			}
		} else {
			this.statusEntry.clear();
		}
	}

	private async listen(): Promise<void> {
		const listen = (globalThis as unknown as ITauriEventGlobal).__TAURI__?.event?.listen;
		if (!listen) {
			return;
		}
		const unlisten = await listen<IMcpRequest>('mcp://request', e => this.handle(e.payload));
		this._register({ dispose: () => unlisten() });
	}

	private async handle(request: IMcpRequest): Promise<void> {
		// Some tools reload or reshape the window; they answer first and act afterwards.
		let after: (() => unknown) | undefined;
		try {
			const result = await this.execute(request.tool, request.args ?? {}, request.client ?? 'unknown', action => { after = action; });
			await tauriInvoke('mcp_respond', { id: request.id, result: result ?? null, error: null });
		} catch (error) {
			await tauriInvoke('mcp_respond', { id: request.id, result: null, error: error instanceof Error ? error.message : String(error) });
			return;
		}
		after?.();
	}

	/** Runs a tool for an MCP client or for the built-in chat agent (same rules and approvals). */
	private async execute(tool: string, args: Record<string, unknown>, client: string, defer: (action: () => unknown) => void): Promise<unknown> {
		if ((UI_TOOLS.has(tool) || WRITE_TOOLS.has(tool)) && !this.trustService.isWorkspaceTrusted()) {
			throw new Error('This workspace is not trusted: AI tools can only read it.');
		}
		const written = await this.writeTools?.run(tool, args, client, {
			resolve: path => this.resolveInWorkspace(path),
			toPath: uri => this.toPath(uri),
			folders: () => this.folders().map(f => f.uri),
		});
		return written !== undefined ? written : this.run(tool, args, defer);
	}

	private folders() {
		return this.contextService.getWorkspace().folders;
	}

	/** Resolves a path and ensures it is inside a workspace folder. */
	private resolveInWorkspace(path: unknown): URI {
		const folders = this.folders();
		if (folders.length === 0) {
			throw new Error('No folder is open in code-rs.');
		}
		const uri = typeof path !== 'string' || path === '' ? folders[0].uri
			: isAbsolute(path) || /^[a-zA-Z]:[\\/]/.test(path) ? URI.file(path)
				: URI.joinPath(folders[0].uri, path);
		if (!folders.some(f => this.uriIdentityService.extUri.isEqualOrParent(uri, f.uri))) {
			throw new Error(`Access denied: ${path} is outside the workspace folders.`);
		}
		return uri;
	}

	private toPath(uri: URI): string {
		return uri.scheme === 'file' ? uri.fsPath : uri.toString();
	}

	private async run(tool: string, args: Record<string, unknown>, defer: (action: () => unknown) => void): Promise<unknown> {
		switch (tool) {
			case 'window_list': {
				const active = this.editorService.activeEditor?.resource;
				return [{
					id: 'main',
					active: true,
					trusted: this.trustService.isWorkspaceTrusted(),
					folders: this.folders().map(f => ({ name: f.name, path: this.toPath(f.uri) })),
					activeFile: active ? this.toPath(active) : null,
				}];
			}
			case 'workspace_open_folder': {
				const folder = URI.file(String(args.path ?? ''));
				await this.fileService.stat(folder);
				defer(() => this.hostService.openWindow([{ folderUri: folder }], { forceReuseWindow: true }));
				return { opening: this.toPath(folder), note: 'Opened in place (no reload); the explorer updates in about 0.1 s.' };
			}
			case 'workspace_add_folder': {
				const folder = URI.file(String(args.path ?? ''));
				await this.fileService.stat(folder);
				await this.workspaceEditingService.addFolders([{ uri: folder }]);
				return { added: this.toPath(folder) };
			}
			case 'workspace_close_folder':
				defer(() => this.commandService.executeCommand('workbench.action.closeFolder'));
				return { closing: true };
			case 'file_read': {
				const uri = this.resolveInWorkspace(args.path);
				const stat = await this.fileService.stat(uri);
				if (stat.isDirectory) {
					throw new Error('Path is a directory; use file_list.');
				}
				if (stat.size > MAX_FILE_BYTES) {
					throw new Error(`File is too large (${stat.size} bytes, limit ${MAX_FILE_BYTES}).`);
				}
				const text = (await this.fileService.readFile(uri)).value.toString();
				const lines = text.split(/\r?\n/);
				const start = Math.max(1, Number(args.startLine ?? 1));
				const end = Math.min(lines.length, Number(args.endLine ?? lines.length));
				return { path: this.toPath(uri), totalLines: lines.length, startLine: start, endLine: end, content: lines.slice(start - 1, end).join('\n') };
			}
			case 'file_list': {
				const uri = this.resolveInWorkspace(args.path);
				const stat = await this.fileService.resolve(uri);
				return {
					path: this.toPath(uri),
					entries: (stat.children ?? []).map(c => ({ name: c.name, type: c.isDirectory ? 'directory' : 'file', size: c.size })),
				};
			}
			case 'editor_state': {
				const control = this.editorService.activeTextEditorControl;
				const active = this.editorService.activeEditor?.resource;
				let selection: unknown = null;
				if (isCodeEditor(control)) {
					const sel = control.getSelection();
					const model = control.getModel();
					if (sel && model) {
						selection = {
							startLine: sel.startLineNumber, startColumn: sel.startColumn,
							endLine: sel.endLineNumber, endColumn: sel.endColumn,
							text: model.getValueInRange(sel).slice(0, 20_000),
						};
					}
				}
				return {
					activeFile: active ? this.toPath(active) : null,
					selection,
					openEditors: this.editorService.editors.map(e => e.resource).filter((r): r is URI => !!r).map(r => this.toPath(r)),
					unsaved: this.editorService.editors.filter(e => e.isDirty()).map(e => e.resource).filter((r): r is URI => !!r).map(r => this.toPath(r)),
				};
			}
			case 'editor_open': {
				const uri = this.resolveInWorkspace(args.path);
				const line = Number(args.line ?? 0);
				const column = Number(args.column ?? 1);
				await this.editorService.openEditor({
					resource: uri,
					options: line > 0 ? { selection: { startLineNumber: line, startColumn: column } } : undefined,
				});
				return { opened: this.toPath(uri) };
			}
			case 'diagnostics_get': {
				const resource = args.path ? this.resolveInWorkspace(args.path) : undefined;
				const markers = this.markerService.read(resource ? { resource } : {})
					.filter(m => !resource || this.folders().some(f => this.uriIdentityService.extUri.isEqualOrParent(m.resource, f.uri)))
					.slice(0, MAX_DIAGNOSTICS);
				const severity = (s: MarkerSeverity) => s === MarkerSeverity.Error ? 'error' : s === MarkerSeverity.Warning ? 'warning' : s === MarkerSeverity.Info ? 'info' : 'hint';
				return markers.map(m => ({
					path: this.toPath(m.resource), severity: severity(m.severity), message: m.message,
					line: m.startLineNumber, column: m.startColumn, source: m.source ?? null,
					code: typeof m.code === 'object' ? m.code.value : m.code ?? null,
				}));
			}
			case 'extensions_audit': {
				const id = String(args.id ?? '');
				const version = typeof args.version === 'string' ? args.version : undefined;
				const [gallery] = await this.galleryService.getExtensions([{ id, version }], CancellationToken.None);
				if (!gallery) {
					throw new Error(`Extension ${id}${version ? '@' + version : ''} not found on Open VSX.`);
				}
				// Audit in quarantine, then discard: nothing is installed.
				const { quarantineId, report } = await tauriInvoke<{ quarantineId: string; report: ITauriExtensionAuditReport }>('extension_quarantine_vsix', {
					request: { url: gallery.assets.download.uri, id: gallery.identifier.id, version: gallery.version },
				});
				await tauriInvoke('extension_quarantine_discard', { quarantineId });
				return report;
			}
			case 'assistant_providers':
				return (await listProviders()).map(p => ({ id: p.id, label: p.label, kind: p.kind, model: p.model, local: p.local, ready: p.hasKey || p.local }));
			case 'assistant_review_extension': {
				const report = await this.run('extensions_audit', args, defer) as ITauriExtensionAuditReport;
				const review = await reviewExtensionReport(this.configurationService, report, typeof args.providerId === 'string' ? args.providerId : undefined);
				if (!review) {
					throw new Error('No extension reviewer is configured (Settings > AI Assistant > Reviewer: Extensions).');
				}
				return { report, review: review.text, provider: review.provider, model: review.model };
			}
			case 'assistant_review_code': {
				const paths = Array.isArray(args.paths) ? args.paths.map(String).slice(0, 20) : [];
				if (!paths.length) {
					throw new Error('paths is required.');
				}
				let content = '';
				for (const p of paths) {
					const file = await this.run('file_read', { path: p }, defer) as { content?: string };
					content += `--- ${p}
${file.content ?? ''}
`;
					if (content.length > 400_000) {
						throw new Error('Too much code for one review (max ~400 KB); pass fewer files.');
					}
				}
				const review = await reviewCode({ configurationService: this.configurationService, dialogService: this.dialogService, storageService: this.storageService }, content, typeof args.focus === 'string' ? args.focus : undefined);
				return { review: review.text, provider: review.provider, model: review.model };
			}
			case 'ui_notify': {
				const sev = args.severity === 'error' ? Severity.Error : args.severity === 'warning' ? Severity.Warning : Severity.Info;
				this.notificationService.notify({ severity: sev, message: `${localize('codeRsMcpFromAi', "AI")}: ${String(args.message ?? '')}`, source: 'code-rs MCP' });
				return { shown: true };
			}
			case 'ui_status': {
				const timeout = Math.min(60_000, Math.max(500, Number(args.timeoutMs ?? 5000)));
				this.notificationService.status(String(args.message ?? ''), { hideAfter: timeout });
				return { shown: true };
			}
			default:
				throw new Error(`Unknown tool: ${tool}`);
		}
	}
}

/** Title bar button (right of the command center): opens the agent chat. Ctrl+Alt+I also does. */
registerAction2(class extends Action2 {
	constructor() {
		super({
			id: 'codeRs.agentChat.open',
			title: localize2('codeRsOpenAgentChat', "Open Agent Chat (Ctrl+Alt+I)"),
			category: localize2('codeRsCategory', "code-rs"),
			icon: Codicon.commentDiscussionSparkle,
			f1: true,
			menu: { id: MenuId.CommandCenter, order: 10003 },
		});
	}

	async run(accessor: ServicesAccessor): Promise<void> {
		await accessor.get(ICommandService).executeCommand('workbench.action.chat.toggle');
	}
});

/** "Ask the AI...": sends a message with the current file and selection to connected MCP clients. */
registerAction2(class extends Action2 {
	constructor() {
		super({
			id: 'codeRs.mcp.askAi',
			title: localize2('codeRsMcpAskAi', "Ask the AI (MCP)..."),
			category: localize2('codeRsCategory', "code-rs"),
			f1: true,
		});
	}

	async run(accessor: ServicesAccessor): Promise<void> {
		const configurationService = accessor.get(IConfigurationService);
		const quickInputService = accessor.get(IQuickInputService);
		const editorService = accessor.get(IEditorService);
		const notificationService = accessor.get(INotificationService);
		if (!isTauri()) {
			return;
		}
		if (configurationService.getValue<boolean>(MCP_ENABLED_SETTING) !== true) {
			notificationService.info(localize('codeRsMcpAskOff', "Turn on the MCP server first (Settings > code-rs MCP)."));
			return;
		}
		const editor = editorService.activeTextEditorControl;
		const model = isCodeEditor(editor) ? editor.getModel() : undefined;
		const selection = isCodeEditor(editor) ? editor.getSelection() : undefined;
		const text = await quickInputService.input({
			title: localize('codeRsMcpAskTitle2', "Message for the connected AI"),
			prompt: localize('codeRsMcpAskPrompt', "Sent with the current file and selection. The AI reads it with the user_messages tool."),
			ignoreFocusLost: true,
		});
		if (!text) {
			return;
		}
		const selected = model && selection && !selection.isEmpty() ? model.getValueInRange(selection).slice(0, 20_000) : undefined;
		await tauriInvoke('mcp_emit_event', {
			kind: 'user-message', data: {
				text, time: Date.now(),
				file: model ? (model.uri.scheme === 'file' ? model.uri.fsPath : model.uri.toString()) : null,
				selection: selection && model ? { startLine: selection.startLineNumber, endLine: selection.endLineNumber, text: selected ?? null } : null,
			}
		});
		notificationService.status(localize('codeRsMcpAskSent', "Sent to the AI. Tell it to check user_messages if it does not pick it up."), { hideAfter: 5000 });
	}
});

/** The MCP panel: status, connection setup and recent activity (Ctrl+Alt+Shift+M). */
registerAction2(class extends Action2 {
	constructor() {
		super({
			id: SHOW_PANEL_COMMAND,
			title: localize2('codeRsMcpShowPanel', "Show MCP Panel (Connections, Approvals and Activity)"),
			category: localize2('codeRsCategory', "code-rs"),
			f1: true,
			// Not used by any built-in VS Code keybinding.
			keybinding: { weight: KeybindingWeight.WorkbenchContrib, primary: KeyMod.CtrlCmd | KeyMod.Alt | KeyMod.Shift | KeyCode.KeyM },
		});
	}

	async run(accessor: ServicesAccessor): Promise<void> {
		const quickInput = accessor.get(IQuickInputService);
		const configurationService = accessor.get(IConfigurationService);
		const clipboard = accessor.get(IClipboardService);
		const notifications = accessor.get(INotificationService);
		if (!isTauri()) {
			return;
		}

		const status = await tauriInvoke<IMcpStatus>('mcp_status');
		const activity = await tauriInvoke<IAuditEntry[]>('mcp_recent_activity', { limit: 30 });
		type Item = IQuickPickItem & { action?: () => unknown };
		const items: (Item | IQuickPickSeparator)[] = [
			{ type: 'separator', label: status.enabled ? localize('codeRsMcpOn', "MCP server is ON · {0}", status.url ?? '') : localize('codeRsMcpOff', "MCP server is OFF") },
			status.enabled
				? { label: '$(debug-stop) ' + localize('codeRsMcpDisable', "Turn off MCP server"), action: () => configurationService.updateValue(MCP_ENABLED_SETTING, false) }
				: { label: '$(plug) ' + localize('codeRsMcpEnable', "Turn on MCP server"), description: localize('codeRsMcpEnableDesc', "local only, token protected"), action: () => configurationService.updateValue(MCP_ENABLED_SETTING, true) },
			{
				label: '$(copy) ' + localize('codeRsMcpCopyConfig', "Copy MCP config for Claude Code / Claude Desktop / Kiro"),
				description: '"command": "code-rs", "args": ["--mcp"]',
				action: async () => {
					await clipboard.writeText(JSON.stringify({ mcpServers: { 'code-rs': { command: status.bridgeCommand, args: ['--mcp'] } } }, undefined, 2));
					notifications.info(localize('codeRsMcpCopied', "MCP configuration copied. Paste it into your AI tool's MCP settings."));
				},
			},
			{ type: 'separator', label: localize('codeRsMcpApprovals', "Pending approvals") },
			{ label: '$(check-all) ' + localize('codeRsMcpNoApprovals', "Nothing to approve"), description: localize('codeRsMcpNoApprovalsDesc', "Changes, installs and commands ask you in a dialog when an AI requests them") },
			{ type: 'separator', label: localize('codeRsMcpActivity', "Recent activity (local audit log)") },
			...(activity.length === 0
				? [{ label: localize('codeRsMcpNoActivity', "No AI activity yet") }]
				: activity.map(a => ({
					label: `${a.ok ? '$(pass)' : '$(error)'} ${a.tool}`,
					description: `${new Date(a.time * 1000).toLocaleString()} · ${a.client}`,
					detail: a.error ?? undefined,
				}))),
		];
		const picked = await quickInput.pick(items, { title: localize('codeRsMcpPanelTitle', "code-rs MCP"), placeHolder: localize('codeRsMcpPanelPlaceholder', "Connections, approvals and activity") }) as Item | undefined;
		await picked?.action?.();
	}
});
