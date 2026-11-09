/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import { CancellationToken } from '../../../../base/common/cancellation.js';
import { Codicon } from '../../../../base/common/codicons.js';
import { MarkdownString } from '../../../../base/common/htmlContent.js';
import { Disposable } from '../../../../base/common/lifecycle.js';
import { localize } from '../../../../nls.js';
import { ConfigurationScope, Extensions as ConfigurationExtensions, IConfigurationRegistry } from '../../../../platform/configuration/common/configurationRegistry.js';
import { IConfigurationService } from '../../../../platform/configuration/common/configuration.js';
import { ExtensionIdentifier } from '../../../../platform/extensions/common/extensions.js';
import { Registry } from '../../../../platform/registry/common/platform.js';
import { isTauri, tauriInvoke } from '../../../../platform/tauri/common/tauri.js';
import { isCodeEditor } from '../../../../editor/browser/editorBrowser.js';
import { IWorkbenchContribution } from '../../../common/contributions.js';
import { IEditorService } from '../../../services/editor/common/editorService.js';
import { IChatAgentHistoryEntry, IChatAgentRequest, IChatAgentResult, IChatAgentService } from '../../chat/common/participants/chatAgents.js';
import { IChatProgress } from '../../chat/common/chatService/chatService.js';
import { ChatAgentLocation, ChatModeKind } from '../../chat/common/constants.js';
import { EFFORT_KEY, providerIdFromModel } from './tauriChatModels.js';
import { IAgentToolHost, IAgentToolHostService } from './tauriAgentTools.js';
import { getExternalAgentRouter } from './tauriExternalAgents.js';
import { CODE_REVIEWER_SETTING, listProviders } from './tauriAssistant.js';

export const CHAT_PROVIDER_SETTING = 'codeRs.assistant.chat.provider';
const DAILY_LIMIT_SETTING = 'codeRs.assistant.dailyTokenLimit';
const CONFIGURE_COMMAND = 'codeRs.assistant.configureProviders';
const MAX_STEPS = 25;

type Block =
	| { type: 'text'; text: string }
	| { type: 'tool_use'; id: string; name: string; input: unknown }
	| { type: 'tool_result'; toolUseId: string; content: string; isError?: boolean };
interface ChatMessage { role: 'user' | 'assistant'; content: Block[] }
interface ChatTurn {
	content: Block[];
	stopReason: 'end_turn' | 'tool_use' | 'max_tokens' | 'refusal' | 'other';
	provider: string; model: string; inputTokens: number; outputTokens: number;
}

const SYSTEM_PROMPT = [
	'You are the code-rs assistant, running inside the code editor.',
	'Read files and inspect the workspace before changing anything.',
	'Change files only through the edit_propose tool; run commands only through the terminal_run tool (the user approves each one).',
	'Be concise. Answer in the language the user writes in.',
].join('\n');

if (isTauri()) {
	Registry.as<IConfigurationRegistry>(ConfigurationExtensions.Configuration).registerConfiguration({
		id: 'codeRsAssistantChat',
		order: 19,
		title: localize('codeRsChatTitle', "AI Assistant Chat"),
		type: 'object',
		properties: {
			[CHAT_PROVIDER_SETTING]: {
				type: 'string',
				default: '',
				scope: ConfigurationScope.APPLICATION,
				markdownDescription: localize('codeRsChatProvider', "AI provider used by the chat view. Empty = the code reviewer provider, or the first configured provider. [Configure AI providers](command:{0})", CONFIGURE_COMMAND),
			},
		}
	});
}

function responseText(response: IChatAgentHistoryEntry['response']): string {
	let out = '';
	for (const part of response) {
		if (part.kind === 'markdownContent') {
			out += part.content.value;
		}
	}
	return out;
}

export class TauriChatAgentContribution extends Disposable implements IWorkbenchContribution {

	static readonly ID = 'workbench.contrib.tauriChatAgent';

	constructor(
		@IChatAgentService private readonly chatAgentService: IChatAgentService,
		@IConfigurationService private readonly configurationService: IConfigurationService,
		@IEditorService private readonly editorService: IEditorService,
		@IAgentToolHostService private readonly toolHost: IAgentToolHost,
	) {
		super();
		if (!isTauri()) {
			return;
		}
		this._register(this.chatAgentService.registerDynamicAgent({
			id: 'code-rs.assistant',
			name: 'code-rs',
			fullName: localize('codeRsChatAgent', "code-rs Assistant"),
			description: localize('codeRsChatAgentDesc', "Ask the code-rs AI assistant."),
			extensionId: new ExtensionIdentifier('code-rs.assistant'),
			extensionVersion: undefined,
			extensionPublisherId: 'code-rs',
			extensionDisplayName: localize('codeRsChatAgent', "code-rs Assistant"),
			isDefault: true, isDynamic: true, isCore: true,
			metadata: { themeIcon: Codicon.sparkle },
			slashCommands: [],
			locations: [ChatAgentLocation.Chat],
			modes: [ChatModeKind.Agent, ChatModeKind.Ask],
			disambiguation: [],
		}, {
			invoke: (request, progress, history, token) => this.invoke(request, progress, history, token),
		}));
	}

	private async resolveProvider(): Promise<string | undefined> {
		const chosen = this.configurationService.getValue<string>(CHAT_PROVIDER_SETTING)
			|| this.configurationService.getValue<string>(CODE_REVIEWER_SETTING);
		if (chosen) {
			return chosen;
		}
		try {
			return (await listProviders())[0]?.id;
		} catch {
			return undefined;
		}
	}

	private editorContext(): string {
		const control = this.editorService.activeTextEditorControl;
		const resource = this.editorService.activeEditor?.resource;
		if (!resource) {
			return '';
		}
		let ctx = `\n\n[Active file: ${resource.scheme === 'file' ? resource.fsPath : resource.toString()}]`;
		if (isCodeEditor(control)) {
			const model = control.getModel();
			const selection = control.getSelection();
			if (model && selection && !selection.isEmpty()) {
				ctx += `\n[Selected text]\n${model.getValueInRange(selection)}`;
			}
		}
		return ctx;
	}

	private async invoke(request: IChatAgentRequest, progress: (parts: IChatProgress[]) => void, history: IChatAgentHistoryEntry[], token: CancellationToken): Promise<IChatAgentResult> {
		if (request.userSelectedModelId?.startsWith('code-rs-agents:')) {
			return getExternalAgentRouter().invoke(request, progress, history, token);
		}
		// The model picker lists our providers (tauriChatModels.ts); its choice wins.
		const providerId = providerIdFromModel(request.userSelectedModelId) ?? await this.resolveProvider();
		const effort = typeof request.modelConfiguration?.[EFFORT_KEY] === 'string' ? request.modelConfiguration[EFFORT_KEY] as string : undefined;
		if (!providerId) {
			const md = new MarkdownString(
				localize('codeRsChatNoProvider', "No AI provider is configured. Run [code-rs: Configure AI Providers]({0}).", `command:${CONFIGURE_COMMAND}`),
				{ isTrusted: { enabledCommands: [CONFIGURE_COMMAND] } });
			progress([{ kind: 'markdownContent', content: md }]);
			return {};
		}

		const messages: ChatMessage[] = [];
		for (const entry of history) {
			messages.push({ role: 'user', content: [{ type: 'text', text: entry.request.message }] });
			const text = responseText(entry.response);
			if (text) {
				messages.push({ role: 'assistant', content: [{ type: 'text', text }] });
			}
		}
		messages.push({ role: 'user', content: [{ type: 'text', text: request.message + this.editorContext() }] });

		try {
			for (let step = 0; step < MAX_STEPS; step++) {
				if (token.isCancellationRequested) {
					return {};
				}
				const turn = await tauriInvoke<ChatTurn>('assistant_chat', {
					providerId,
					system: SYSTEM_PROMPT,
					messages,
					tools: this.toolHost.tools(),
					maxTokens: 8000,
					effort,
					dailyTokenLimit: this.configurationService.getValue<number>(DAILY_LIMIT_SETTING) ?? 0,
				});
				if (token.isCancellationRequested) {
					return {};
				}
				const toolUses: Extract<Block, { type: 'tool_use' }>[] = [];
				for (const block of turn.content) {
					if (block.type === 'text' && block.text) {
						progress([{ kind: 'markdownContent', content: new MarkdownString(block.text) }]);
					} else if (block.type === 'tool_use') {
						toolUses.push(block);
					}
				}
				messages.push({ role: 'assistant', content: turn.content });

				if (turn.stopReason === 'refusal') {
					progress([{ kind: 'warning', content: new MarkdownString(localize('codeRsChatRefusal', "The model refused to answer this request.")) }]);
					return {};
				}
				if (turn.stopReason === 'max_tokens') {
					progress([{ kind: 'warning', content: new MarkdownString(localize('codeRsChatMaxTokens', "The response was cut off (token limit reached).")) }]);
					return {};
				}
				if (toolUses.length === 0) {
					return {};
				}

				const results: Block[] = [];
				for (const use of toolUses) {
					progress([{ kind: 'progressMessage', content: new MarkdownString(localize('codeRsChatTool', "Running `{0}`...", use.name)) }]);
					try {
						const out = await this.toolHost.run(use.name, (use.input ?? {}) as Record<string, unknown>, token);
						results.push({ type: 'tool_result', toolUseId: use.id, content: typeof out === 'string' ? out : JSON.stringify(out) ?? '' });
					} catch (e) {
						results.push({ type: 'tool_result', toolUseId: use.id, content: e instanceof Error ? e.message : String(e), isError: true });
					}
				}
				messages.push({ role: 'user', content: results });
			}
			progress([{ kind: 'warning', content: new MarkdownString(localize('codeRsChatMaxSteps', "Stopped after {0} steps.", MAX_STEPS)) }]);
			return {};
		} catch (e) {
			return { errorDetails: { message: e instanceof Error ? e.message : String(e) } };
		}
	}
}
