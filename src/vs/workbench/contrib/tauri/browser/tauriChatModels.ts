/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import { CancellationToken } from '../../../../base/common/cancellation.js';
import { Emitter } from '../../../../base/common/event.js';
import { Disposable, toDisposable } from '../../../../base/common/lifecycle.js';
import { localize } from '../../../../nls.js';
import { IConfigurationService } from '../../../../platform/configuration/common/configuration.js';
import { nullExtensionDescription } from '../../../services/extensions/common/extensions.js';
import { isTauri } from '../../../../platform/tauri/common/tauri.js';
import { IWorkbenchContribution } from '../../../common/contributions.js';
import { IChatMessage, ILanguageModelChatMetadataAndIdentifier, ILanguageModelChatProvider, ILanguageModelChatResponse, ILanguageModelConfigurationSchema, ILanguageModelsService } from '../../chat/common/languageModels.js';
import { ChatAgentLocation } from '../../chat/common/constants.js';
import { complete, IAssistantProvider, listProviders, onDidChangeProviders, ProviderKind } from './tauriAssistant.js';

/**
 * Our AI providers as models of the VS Code chat (the model picker, next to "Auto"), each with
 * an effort dropdown when the provider supports one. The chat agent (tauriChatAgent.ts) reads
 * the selected model (`code-rs:<providerId>`) and its `effort` from the request.
 */

export const CHAT_MODEL_VENDOR = 'code-rs';
export const EFFORT_KEY = 'effort';

/** Effort levels per provider family; undefined = the provider has no effort control. */
export function effortLevels(kind: ProviderKind, model: string): string[] | undefined {
	switch (kind) {
		case 'anthropic': return ['low', 'medium', 'high', 'xhigh', 'max'];
		case 'openai': return /^(o\d|gpt-5)/.test(model) ? ['low', 'medium', 'high'] : undefined;
		case 'openrouter': return ['low', 'medium', 'high'];
		default: return undefined; // local / OpenAI-compatible servers usually ignore it
	}
}

/** Provider id from a chat model identifier (`code-rs:<providerId>`). */
export function providerIdFromModel(identifier: string | undefined): string | undefined {
	return identifier?.startsWith(`${CHAT_MODEL_VENDOR}:`) ? identifier.slice(CHAT_MODEL_VENDOR.length + 1) : undefined;
}

class CodeRsModelProvider extends Disposable implements ILanguageModelChatProvider {

	private readonly _onDidChange = this._register(new Emitter<void>());
	readonly onDidChange = this._onDidChange.event;

	constructor(private readonly configurationService: IConfigurationService) {
		super();
		this._register(onDidChangeProviders(() => this._onDidChange.fire()));
	}

	async provideLanguageModelChatInfo(): Promise<ILanguageModelChatMetadataAndIdentifier[]> {
		let providers: IAssistantProvider[] = [];
		try {
			providers = await listProviders();
		} catch {
			return [];
		}
		return providers.map((p, i) => {
			const levels = effortLevels(p.kind, p.model);
			const configurationSchema: ILanguageModelConfigurationSchema | undefined = levels && {
				type: 'object',
				properties: {
					[EFFORT_KEY]: {
						type: 'string',
						title: localize('codeRsEffort', "Effort"),
						enum: levels,
						enumItemLabels: levels.map(l => l === 'xhigh' ? 'Extra high' : l[0].toUpperCase() + l.slice(1)),
						default: levels.includes('medium') ? 'medium' : levels[0],
						group: 'navigation',
					}
				}
			};
			return {
				identifier: `${CHAT_MODEL_VENDOR}:${p.id}`,
				metadata: {
					extension: nullExtensionDescription.identifier,
					name: p.label,
					id: p.id,
					vendor: CHAT_MODEL_VENDOR,
					version: '1.0',
					family: p.model,
					detail: p.model,
					tooltip: `${p.label} · ${p.model}${p.local ? ' · ' + localize('codeRsLocalModel', "local") : ''}`,
					maxInputTokens: 200_000,
					maxOutputTokens: 8_000,
					isDefaultForLocation: i === 0 ? { [ChatAgentLocation.Chat]: true } : {},
					isUserSelectable: true,
					capabilities: { toolCalling: true, agentMode: true },
					...(configurationSchema ? { configurationSchema } : {}),
				},
			};
		});
	}

	/** Plain completion (used by features that call a model directly, not by the agent). */
	async sendChatRequest(modelId: string, messages: IChatMessage[], _from: unknown, _options: unknown, _token: CancellationToken): Promise<ILanguageModelChatResponse> {
		const text = (m: IChatMessage) => m.content.map(c => c.type === 'text' ? c.value : '').join('');
		const system = messages.filter(m => m.role === 0 /* System */).map(text).join('\n');
		const user = messages.filter(m => m.role !== 0).map(text).join('\n\n');
		const done = complete(this.configurationService, providerIdFromModel(modelId) ?? modelId, system, user);
		return {
			stream: (async function* () { yield { type: 'text' as const, value: (await done).text }; })(),
			result: done.then(() => ({})),
		};
	}

	async provideTokenCount(_modelId: string, message: string | IChatMessage): Promise<number> {
		const s = typeof message === 'string' ? message : JSON.stringify(message.content);
		return Math.ceil(s.length / 4);
	}
}

export class TauriChatModelsContribution extends Disposable implements IWorkbenchContribution {

	static readonly ID = 'workbench.contrib.tauriChatModels';

	constructor(
		@ILanguageModelsService languageModelsService: ILanguageModelsService,
		@IConfigurationService configurationService: IConfigurationService,
	) {
		super();
		if (!isTauri()) {
			return;
		}
		const descriptor = { vendor: CHAT_MODEL_VENDOR, displayName: 'code-rs', configuration: undefined, managementCommand: 'codeRs.assistant.configureProviders', when: undefined };
		languageModelsService.deltaLanguageModelChatProviderDescriptors([descriptor], []);
		this._register(toDisposable(() => languageModelsService.deltaLanguageModelChatProviderDescriptors([], [descriptor])));
		this._register(languageModelsService.registerLanguageModelProvider(CHAT_MODEL_VENDOR, this._register(new CodeRsModelProvider(configurationService))));
	}
}
