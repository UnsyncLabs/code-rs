/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import { CancellationToken } from '../../../../base/common/cancellation.js';
import { Emitter } from '../../../../base/common/event.js';
import { MarkdownString } from '../../../../base/common/htmlContent.js';
import { Disposable, DisposableStore, IDisposable, toDisposable } from '../../../../base/common/lifecycle.js';
import { isEqualOrParent } from '../../../../base/common/extpath.js';
import { URI } from '../../../../base/common/uri.js';
import { VSBuffer } from '../../../../base/common/buffer.js';
import { localize, localize2 } from '../../../../nls.js';
import { Action2, registerAction2 } from '../../../../platform/actions/common/actions.js';
import { IFileService } from '../../../../platform/files/common/files.js';
import { ServicesAccessor } from '../../../../platform/instantiation/common/instantiation.js';
import { INotificationService } from '../../../../platform/notification/common/notification.js';
import { IOpenerService } from '../../../../platform/opener/common/opener.js';
import { IQuickInputService } from '../../../../platform/quickinput/common/quickInput.js';
import { isTauri, tauriInvoke } from '../../../../platform/tauri/common/tauri.js';
import { IWorkspaceContextService } from '../../../../platform/workspace/common/workspace.js';
import { IWorkbenchContribution } from '../../../common/contributions.js';
import { nullExtensionDescription } from '../../../services/extensions/common/extensions.js';
import { IChatAgentHistoryEntry, IChatAgentRequest, IChatAgentResult } from '../../chat/common/participants/chatAgents.js';
import { IChatProgress } from '../../chat/common/chatService/chatService.js';
import { IChatMessage, ILanguageModelChatMetadataAndIdentifier, ILanguageModelChatProvider, ILanguageModelChatResponse, ILanguageModelsService } from '../../chat/common/languageModels.js';
import { IAgentToolHost, IAgentToolHostService } from './tauriAgentTools.js';
import { ITauriApprovalService } from './tauriApprovals.js';

/**
 * External agents (Muse, Antigravity) as entries of the chat model picker. The chat agent
 * (tauriChatAgent.ts) routes requests whose model starts with `code-rs-agents:` to the
 * {@link ExternalAgentRouter}.
 */

export const EXTERNAL_AGENTS_VENDOR = 'code-rs-agents';
const PAIR_COMMAND = 'codeRs.muse.pair';
const UNPAIR_COMMAND = 'codeRs.muse.unpair';
const SDK_TOKENS_URL = 'https://gadgets.muse.ai/settings/sdk-tokens';

interface ITauriEventGlobal {
	__TAURI__?: { event?: { listen<T>(event: string, handler: (e: { payload: T }) => void): Promise<() => void> } };
}

interface IMuseStatus { paired: boolean; connected: boolean; deviceName?: string; error?: string }
interface IMuseReply { text: string; sessionId?: string }
interface IMuseCommandEvent { callId: string; name: string; args: Record<string, unknown> }
interface IAcpAgentInfo { id: string; label: string; available: boolean }

interface IAcpOption { optionId: string; name: string; kind: string }
interface IAcpToolCall { toolCallId?: string; title?: string; kind?: string; status?: string }
interface IAcpUpdateEvent { sessionId: string; update: { sessionUpdate?: string; content?: { type?: string; text?: string } } & IAcpToolCall }
interface IAcpPermissionEvent { sessionId: string; requestId: string; toolCall?: IAcpToolCall; options: IAcpOption[] }
interface IAcpFsEvent { sessionId: string; requestId: string; op: 'read' | 'write'; path: string; content?: string; line?: number; limit?: number }

async function listen<T>(event: string, handler: (payload: T) => void): Promise<IDisposable> {
	const fn = (globalThis as unknown as ITauriEventGlobal).__TAURI__?.event?.listen;
	if (!fn) {
		return toDisposable(() => { });
	}
	const unlisten = await fn<T>(event, e => handler(e.payload));
	return toDisposable(() => unlisten());
}

const errorMessage = (e: unknown) => e instanceof Error ? e.message : String(e);

interface IAcpSession {
	acpId: string;
	/** Set while a turn is running. */
	progress?: (parts: IChatProgress[]) => void;
}

export class ExternalAgentRouter extends Disposable {

	private readonly museSessions = new Map<string, string>();
	private museReady: Promise<void> | undefined;
	private readonly acpSessions = new Map<string, IAcpSession>(); // by chat session
	private readonly acpById = new Map<string, IAcpSession>(); // by ACP session id
	private acpListening: Promise<void> | undefined;

	constructor(
		private readonly toolHost: IAgentToolHost,
		private readonly approvals: ITauriApprovalService,
		private readonly fileService: IFileService,
		private readonly contextService: IWorkspaceContextService,
	) {
		super();
	}

	async invoke(request: IChatAgentRequest, progress: (parts: IChatProgress[]) => void, _history: IChatAgentHistoryEntry[], token: CancellationToken): Promise<IChatAgentResult> {
		try {
			const model = request.userSelectedModelId?.slice(EXTERNAL_AGENTS_VENDOR.length + 1);
			if (model === 'muse') {
				await this.invokeMuse(request, progress);
			} else if (model === 'antigravity') {
				await this.invokeAntigravity(request, progress, token);
			} else {
				progress([{ kind: 'warning', content: new MarkdownString(localize('codeRsAgentsUnknown', "Unknown external agent.")) }]);
			}
			return {};
		} catch (e) {
			return { errorDetails: { message: errorMessage(e) } };
		}
	}

	// --- Muse ---

	private async invokeMuse(request: IChatAgentRequest, progress: (parts: IChatProgress[]) => void): Promise<void> {
		const status = await tauriInvoke<IMuseStatus>('muse_status');
		if (!status.paired) {
			progress([{
				kind: 'markdownContent',
				content: new MarkdownString(
					localize('codeRsMuseNotPaired', "Muse is not paired yet. Run the command [code-rs: Pair Muse]({0}) (Ctrl+Shift+P) and paste your SDK token.", `command:${PAIR_COMMAND}`),
					{ isTrusted: { enabledCommands: [PAIR_COMMAND] } })
			}]);
			return;
		}
		await this.ensureMuseReady();
		const key = request.sessionResource.toString();
		const reply = await tauriInvoke<IMuseReply>('muse_send', { text: request.message, sessionId: this.museSessions.get(key) });
		if (reply.sessionId) {
			this.museSessions.set(key, reply.sessionId);
		}
		progress([{ kind: 'markdownContent', content: new MarkdownString(reply.text) }]);
	}

	private ensureMuseReady(): Promise<void> {
		this.museReady ??= (async () => {
			const commands = this.toolHost.tools().map(t => ({ name: t.name, description: t.description, inputSchema: t.inputSchema }));
			await tauriInvoke('muse_set_commands', { commands });
			this._register(await listen<IMuseCommandEvent>('muse://command', e => this.runMuseCommand(e)));
		})().catch(err => { this.museReady = undefined; throw err; });
		return this.museReady;
	}

	private async runMuseCommand(e: IMuseCommandEvent): Promise<void> {
		let ok = true;
		let result: unknown;
		try {
			result = await this.toolHost.run(e.name, e.args ?? {}, CancellationToken.None);
		} catch (err) {
			ok = false;
			result = errorMessage(err);
		}
		try {
			await tauriInvoke('muse_command_result', { callId: e.callId, ok, result: result ?? null });
		} catch { /* link closed */ }
	}

	// --- Antigravity (ACP) ---

	private ensureAcpListeners(): Promise<void> {
		this.acpListening ??= (async () => {
			this._register(await listen<IAcpUpdateEvent>('acp://update', e => this.onAcpUpdate(e)));
			this._register(await listen<IAcpPermissionEvent>('acp://permission', e => this.onAcpPermission(e)));
			this._register(await listen<IAcpFsEvent>('acp://fs', e => this.onAcpFs(e)));
			this._register(await listen<{ sessionId: string }>('acp://exit', e => {
				const s = this.acpById.get(e.sessionId);
				this.acpById.delete(e.sessionId);
				for (const [k, v] of this.acpSessions) {
					if (v === s) {
						this.acpSessions.delete(k);
					}
				}
			}));
		})().catch(err => { this.acpListening = undefined; throw err; });
		return this.acpListening;
	}

	private async invokeAntigravity(request: IChatAgentRequest, progress: (parts: IChatProgress[]) => void, token: CancellationToken): Promise<void> {
		const agents = await tauriInvoke<IAcpAgentInfo[]>('acp_agents');
		if (!agents.find(a => a.id === 'antigravity')?.available) {
			progress([{ kind: 'warning', content: new MarkdownString(localize('codeRsAntigravityMissing', "The Antigravity CLI (`agy`) was not found on PATH.")) }]);
			return;
		}
		await this.ensureAcpListeners();
		const key = request.sessionResource.toString();
		let session = this.acpSessions.get(key);
		if (!session) {
			const cwd = this.contextService.getWorkspace().folders[0]?.uri.fsPath;
			const acpId = await tauriInvoke<string>('acp_start', { agentId: 'antigravity', cwd });
			session = { acpId };
			this.acpSessions.set(key, session);
			this.acpById.set(acpId, session);
		}
		const active = session;
		active.progress = progress;
		const cancel = token.onCancellationRequested(() => { tauriInvoke('acp_cancel', { sessionId: active.acpId }).catch(() => { }); });
		try {
			await tauriInvoke('acp_prompt', { sessionId: active.acpId, text: request.message });
		} finally {
			cancel.dispose();
			active.progress = undefined;
		}
	}

	private onAcpUpdate(e: IAcpUpdateEvent): void {
		const session = this.acpById.get(e.sessionId);
		const u = e.update;
		if (!session?.progress || !u) {
			return;
		}
		switch (u.sessionUpdate) {
			case 'agent_message_chunk':
				if (u.content?.type === 'text' && u.content.text) {
					session.progress([{ kind: 'markdownContent', content: new MarkdownString(u.content.text) }]);
				}
				break;
			case 'tool_call':
			case 'tool_call_update': {
				const title = u.title ?? u.kind ?? 'tool';
				const text = u.status === 'completed' ? localize('codeRsAcpToolDone', "Done: {0}", title)
					: u.status === 'failed' ? localize('codeRsAcpToolFailed', "Failed: {0}", title)
						: localize('codeRsAcpToolRunning', "Running {0}...", title);
				session.progress([{ kind: 'progressMessage', content: new MarkdownString(text) }]);
				break;
			}
		}
	}

	private async onAcpPermission(e: IAcpPermissionEvent): Promise<void> {
		const tool = e.toolCall?.title ?? e.toolCall?.kind ?? 'tool';
		let optionId: string | undefined;
		try {
			const choice = await this.approvals.request({
				client: 'Antigravity', tool, level: 3,
				title: localize('codeRsAcpPermTitle', "Antigravity wants to use: {0}", tool),
				detail: new MarkdownString(e.toolCall?.kind ? '`' + e.toolCall.kind + '`' : ''),
			});
			const kinds = choice === 'session' ? ['allow_always', 'allow_once'] : choice === 'once' ? ['allow_once', 'allow_always'] : ['reject_once', 'reject_always'];
			optionId = kinds.map(k => e.options.find(o => o.kind === k)).find(o => !!o)?.optionId;
		} catch { /* treated as cancelled */ }
		await tauriInvoke('acp_permission_result', { requestId: e.requestId, optionId, cancelled: !optionId }).catch(() => { });
	}

	private async onAcpFs(e: IAcpFsEvent): Promise<void> {
		const answer = async (r: { ok: boolean; content?: string; error?: string }): Promise<void> => { await tauriInvoke('acp_fs_result', { requestId: e.requestId, ...r }).catch(() => { }); };
		try {
			const uri = URI.file(e.path);
			const folders = this.contextService.getWorkspace().folders;
			if (!folders.some(f => isEqualOrParent(uri.fsPath, f.uri.fsPath, true))) {
				return answer({ ok: false, error: `Access denied: ${e.path} is outside the workspace folders.` });
			}
			if (e.op === 'read') {
				const text = (await this.fileService.readFile(uri)).value.toString();
				let content = text;
				if (e.line || e.limit) {
					const lines = text.split(/\r?\n/);
					const start = Math.max(0, (e.line ?? 1) - 1);
					content = lines.slice(start, e.limit ? start + e.limit : undefined).join('\n');
				}
				return answer({ ok: true, content });
			}
			const choice = await this.approvals.request({
				client: 'Antigravity', tool: 'fs/write_text_file', level: 2,
				title: localize('codeRsAcpWriteTitle', "Antigravity wants to write a file"),
				detail: new MarkdownString('`' + e.path + '`'),
			});
			if (choice === 'deny') {
				return answer({ ok: false, error: 'The user denied the write.' });
			}
			await this.fileService.writeFile(uri, VSBuffer.fromString(e.content ?? ''));
			return answer({ ok: true });
		} catch (err) {
			return answer({ ok: false, error: errorMessage(err) });
		}
	}

	override dispose(): void {
		for (const s of this.acpById.values()) {
			tauriInvoke('acp_stop', { sessionId: s.acpId }).catch(() => { });
		}
		super.dispose();
	}
}

let router: ExternalAgentRouter | undefined;

export function getExternalAgentRouter(): ExternalAgentRouter {
	if (!router) {
		throw new Error('External agents are not available yet.');
	}
	return router;
}

class ExternalAgentsProvider extends Disposable implements ILanguageModelChatProvider {

	private readonly _onDidChange = this._register(new Emitter<void>());
	readonly onDidChange = this._onDidChange.event;

	refresh(): void {
		this._onDidChange.fire();
	}

	async provideLanguageModelChatInfo(): Promise<ILanguageModelChatMetadataAndIdentifier[]> {
		let antigravity = false;
		try {
			antigravity = !!(await tauriInvoke<IAcpAgentInfo[]>('acp_agents')).find(a => a.id === 'antigravity')?.available;
		} catch { /* ACP unavailable */ }
		const entries = [{ id: 'muse', name: 'Muse (Meta)' }, ...(antigravity ? [{ id: 'antigravity', name: 'Antigravity (Google)' }] : [])];
		return entries.map(m => ({
			identifier: `${EXTERNAL_AGENTS_VENDOR}:${m.id}`,
			metadata: {
				extension: nullExtensionDescription.identifier,
				name: m.name,
				id: m.id,
				vendor: EXTERNAL_AGENTS_VENDOR,
				version: '1.0',
				family: m.id,
				detail: m.name,
				tooltip: m.name,
				maxInputTokens: 200_000,
				maxOutputTokens: 8_000,
				isDefaultForLocation: {},
				isUserSelectable: true,
				capabilities: { toolCalling: true, agentMode: true },
			},
		}));
	}

	async sendChatRequest(_modelId: string, _messages: IChatMessage[]): Promise<ILanguageModelChatResponse> {
		throw new Error('External agents only work through the chat view.');
	}

	async provideTokenCount(_modelId: string, message: string | IChatMessage): Promise<number> {
		const s = typeof message === 'string' ? message : JSON.stringify(message.content);
		return Math.ceil(s.length / 4);
	}
}

export class TauriExternalAgentsContribution extends Disposable implements IWorkbenchContribution {

	static readonly ID = 'workbench.contrib.tauriExternalAgents';

	constructor(
		@ILanguageModelsService languageModelsService: ILanguageModelsService,
		@IAgentToolHostService toolHost: IAgentToolHost,
		@ITauriApprovalService approvals: ITauriApprovalService,
		@IFileService fileService: IFileService,
		@IWorkspaceContextService contextService: IWorkspaceContextService,
	) {
		super();
		if (!isTauri()) {
			return;
		}
		const descriptor = { vendor: EXTERNAL_AGENTS_VENDOR, displayName: 'code-rs agents', configuration: undefined, managementCommand: undefined, when: undefined };
		languageModelsService.deltaLanguageModelChatProviderDescriptors([descriptor], []);
		this._register(toDisposable(() => languageModelsService.deltaLanguageModelChatProviderDescriptors([], [descriptor])));
		const provider = this._register(new ExternalAgentsProvider());
		this._register(languageModelsService.registerLanguageModelProvider(EXTERNAL_AGENTS_VENDOR, provider));
		provider.refresh();
		router = this._register(new ExternalAgentRouter(toolHost, approvals, fileService, contextService));
		this._register(toDisposable(() => { router = undefined; }));
	}
}

registerAction2(class extends Action2 {
	constructor() {
		super({ id: PAIR_COMMAND, title: localize2('codeRsMusePair', "code-rs: Pair Muse"), f1: true });
	}

	async run(accessor: ServicesAccessor): Promise<void> {
		if (!isTauri()) {
			return;
		}
		const quickInput = accessor.get(IQuickInputService);
		const notifications = accessor.get(INotificationService);
		const opener = accessor.get(IOpenerService);
		const store = new DisposableStore();
		try {
			const input = store.add(quickInput.createInputBox());
			input.title = localize('codeRsMusePairTitle', "Pair Muse");
			input.password = true;
			input.ignoreFocusOut = true;
			input.placeholder = localize('codeRsMusePairPlaceholder', "Paste your Muse SDK token");
			input.prompt = localize('codeRsMusePairPrompt', "The token is personal and is kept in your system keychain. Create one at {0}", SDK_TOKENS_URL);
			input.buttons = [{ iconClass: 'codicon codicon-link-external', tooltip: localize('codeRsMuseOpenTokens', "Open the SDK tokens page") }];
			store.add(input.onDidTriggerButton(() => opener.open(URI.parse(SDK_TOKENS_URL))));
			const token = await new Promise<string | undefined>(resolve => {
				store.add(input.onDidAccept(() => resolve(input.value.trim())));
				store.add(input.onDidHide(() => resolve(undefined)));
				input.show();
			});
			input.hide();
			if (!token) {
				return;
			}
			await tauriInvoke('muse_pair', { sdkToken: token });
			notifications.info(localize('codeRsMusePaired', "Muse is paired. Pick \"Muse (Meta)\" in the chat model picker."));
		} catch (e) {
			notifications.error(localize('codeRsMusePairFailed', "Could not pair Muse: {0}", errorMessage(e)));
		} finally {
			store.dispose();
		}
	}
});

registerAction2(class extends Action2 {
	constructor() {
		super({ id: UNPAIR_COMMAND, title: localize2('codeRsMuseUnpair', "code-rs: Unpair Muse"), f1: true });
	}

	async run(accessor: ServicesAccessor): Promise<void> {
		if (!isTauri()) {
			return;
		}
		const notifications = accessor.get(INotificationService);
		try {
			await tauriInvoke('muse_unpair');
			notifications.info(localize('codeRsMuseUnpaired', "Muse was unpaired."));
		} catch (e) {
			notifications.error(localize('codeRsMuseUnpairFailed', "Could not unpair Muse: {0}", errorMessage(e)));
		}
	}
});
