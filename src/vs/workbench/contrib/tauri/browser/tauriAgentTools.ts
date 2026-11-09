/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import { CancellationToken } from '../../../../base/common/cancellation.js';
import { CancellationError } from '../../../../base/common/errors.js';
import { InstantiationType, registerSingleton } from '../../../../platform/instantiation/common/extensions.js';
import { createDecorator } from '../../../../platform/instantiation/common/instantiation.js';
import { isTauri, tauriInvoke } from '../../../../platform/tauri/common/tauri.js';

/**
 * Tools of the built-in chat agent (contract C, see the chat design notes). They are the
 * MCP tools, run by the same code with the same approvals, so the chat agent can never do
 * more than an external MCP client could.
 */

export interface ToolSpec {
	readonly name: string;
	readonly description: string;
	readonly inputSchema: object;
}

export interface IAgentToolHost {
	tools(): ToolSpec[];
	run(name: string, input: Record<string, unknown>, token: CancellationToken): Promise<unknown>;
}

export const IAgentToolHostService = createDecorator<IAgentToolHost & { readonly _serviceBrand: undefined }>('agentToolHost');

/** Client name shown in approval cards for calls made by the chat agent. */
export const AGENT_CLIENT_NAME = 'code-rs agent';

/** Tools that make no sense for the chat agent (they reshape the window it lives in, or are MCP-only). */
const EXCLUDED = new Set(['workspace_open_folder', 'workspace_add_folder', 'workspace_close_folder', 'user_messages', 'ui_status', 'ui_notify', 'window_list']);

type Executor = (tool: string, args: Record<string, unknown>, client: string) => Promise<unknown>;
let executor: Executor | undefined;

/** Set by the MCP contribution, which owns the tool implementations. */
export function setAgentToolExecutor(fn: Executor | undefined): void {
	executor = fn;
}

class AgentToolHost implements IAgentToolHost {

	declare readonly _serviceBrand: undefined;

	private specs: ToolSpec[] = [];
	private readonly loaded: Promise<void>;

	constructor() {
		this.loaded = isTauri()
			? tauriInvoke<ToolSpec[]>('mcp_tools').then(all => { this.specs = all.filter(t => !EXCLUDED.has(t.name)); }, () => { /* no tools */ })
			: Promise.resolve();
	}

	tools(): ToolSpec[] {
		return this.specs;
	}

	/** Resolves once the tool list is loaded (the chat agent awaits it before its first call). */
	whenReady(): Promise<void> {
		return this.loaded;
	}

	async run(name: string, input: Record<string, unknown>, token: CancellationToken): Promise<unknown> {
		await this.loaded;
		if (!this.specs.some(t => t.name === name)) {
			throw new Error(`Unknown tool: ${name}`);
		}
		if (token.isCancellationRequested) {
			throw new CancellationError();
		}
		if (name === 'command_assess') {
			return tauriInvoke('guardian_assess', { command: String(input.command ?? '') });
		}
		if (!executor) {
			throw new Error('code-rs tools are not ready yet.');
		}
		return executor(name, input, AGENT_CLIENT_NAME);
	}
}

registerSingleton(IAgentToolHostService, AgentToolHost, InstantiationType.Eager);
