/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import * as dom from '../../../../base/browser/dom.js';
import { Button } from '../../../../base/browser/ui/button/button.js';
import { Checkbox } from '../../../../base/browser/ui/toggle/toggle.js';
import { Codicon } from '../../../../base/common/codicons.js';
import { Emitter, Event } from '../../../../base/common/event.js';
import { IMarkdownString, MarkdownString } from '../../../../base/common/htmlContent.js';
import { Disposable } from '../../../../base/common/lifecycle.js';
import { ThemeIcon } from '../../../../base/common/themables.js';
import Severity from '../../../../base/common/severity.js';
import { localize } from '../../../../nls.js';
import { IDialogService } from '../../../../platform/dialogs/common/dialogs.js';
import { InstantiationType, registerSingleton } from '../../../../platform/instantiation/common/extensions.js';
import { createDecorator } from '../../../../platform/instantiation/common/instantiation.js';
import { IMarkdownRendererService } from '../../../../platform/markdown/browser/markdownRenderer.js';
import { defaultButtonStyles, defaultCheckboxStyles } from '../../../../platform/theme/browser/defaultStyles.js';
import { IView } from '../../../common/views.js';
import { IViewsService } from '../../../services/views/common/viewsService.js';
import { ChatViewId } from '../../chat/browser/chat.js';
import './media/tauriApprovals.css';

/**
 * Approvals broker (shared contracts, Contract B). Every tool call that needs the user's
 * consent — from this app's chat agent and from external MCP clients (Kiro, Claude Code) —
 * is routed through here. When the chat view is open the request is shown as a card docked
 * above the chat input; otherwise it falls back to the modal dialog that
 * `tauriMcpWriteTools.confirm()` used to show.
 */

export type ApprovalChoice = 'once' | 'session' | 'deny';

export interface IApprovalRequest {
	readonly client: string;          // "Kiro", "claude-code", "code-rs agent"
	readonly tool: string;            // e.g. edit_propose, terminal_run
	readonly level: 2 | 3;            // 2 offers "Allow for this session"; 3 never does
	readonly title: string;           // one line
	readonly detail?: IMarkdownString; // command, diff summary, guardian findings...
	readonly highRisk?: boolean;      // requires an "I understand the risk" checkbox to allow
}

export const ITauriApprovalService = createDecorator<ITauriApprovalService>('tauriApprovalService');

export interface ITauriApprovalService {
	readonly _serviceBrand: undefined;
	request(req: IApprovalRequest): Promise<ApprovalChoice>;
	/** Pending requests, for the MCP panel. */
	readonly pending: readonly IApprovalRequest[];
	readonly onDidChangePending: Event<void>;
}

/** The chat view pane exposes its root element (from the base `Pane`); `IView` does not. */
type IChatViewWithElement = IView & { readonly element: HTMLElement };

/** One in-flight request plus the plumbing to resolve it and tear its card down. */
interface IPendingApproval {
	readonly req: IApprovalRequest;
	readonly resolve: (choice: ApprovalChoice) => void;
	/** Set while the request is shown as a card; cleared when it moves to the modal. */
	card?: ApprovalCard;
	/** True once resolved, so a view-visibility change cannot double-resolve it. */
	settled: boolean;
}

export class TauriApprovalService extends Disposable implements ITauriApprovalService {

	declare readonly _serviceBrand: undefined;

	private readonly _pending: IPendingApproval[] = [];

	private readonly _onDidChangePending = this._register(new Emitter<void>());
	readonly onDidChangePending: Event<void> = this._onDidChangePending.event;

	constructor(
		@IViewsService private readonly viewsService: IViewsService,
		@IDialogService private readonly dialogService: IDialogService,
		@IMarkdownRendererService private readonly markdownRendererService: IMarkdownRendererService,
	) {
		super();

		// If the chat view is hidden while a card is pending, the card becomes invisible;
		// move those requests to the modal dialog so the user can still answer them.
		this._register(this.viewsService.onDidChangeViewVisibility(e => {
			if (e.id === ChatViewId && !e.visible) {
				this.rehomePendingCardsToModal();
			}
		}));
	}

	get pending(): readonly IApprovalRequest[] {
		return this._pending.map(p => p.req);
	}

	async request(req: IApprovalRequest): Promise<ApprovalChoice> {
		let resolve!: (choice: ApprovalChoice) => void;
		const promise = new Promise<ApprovalChoice>(r => { resolve = r; });

		const entry: IPendingApproval = { req, resolve, settled: false };
		this._pending.push(entry);
		this._onDidChangePending.fire();

		const inputContainer = this.findChatInputContainer();
		if (inputContainer) {
			this.showCard(entry, inputContainer);
		} else {
			this.showModal(entry);
		}

		return promise;
	}

	/** Resolves a request exactly once, removing it from `pending` and tearing down its card. */
	private settle(entry: IPendingApproval, choice: ApprovalChoice): void {
		if (entry.settled) {
			return;
		}
		entry.settled = true;
		const index = this._pending.indexOf(entry);
		if (index >= 0) {
			this._pending.splice(index, 1);
		}
		entry.card?.dispose();
		entry.card = undefined;
		entry.resolve(choice);
		this._onDidChangePending.fire();
	}

	/**
	 * Locates the `.interactive-input-part` inside the chat view pane so cards can be docked
	 * right above the chat input. Returns `undefined` when the chat view is not visible.
	 */
	private findChatInputContainer(): HTMLElement | undefined {
		if (!this.viewsService.isViewVisible(ChatViewId)) {
			return undefined;
		}
		const view = this.viewsService.getActiveViewWithId<IChatViewWithElement>(ChatViewId);
		const root = view?.element;
		if (!root) {
			return undefined;
		}
		// eslint-disable-next-line no-restricted-syntax -- the chat input belongs to the chat view; no handle is exposed
		return root.querySelector<HTMLElement>('.interactive-input-part') ?? undefined;
	}

	/** Shows a request as a card stacked (oldest first) directly above the chat input. */
	private showCard(entry: IPendingApproval, inputContainer: HTMLElement): void {
		// Reuse one host element right before the chat input; cards stack inside it.
		const parent = inputContainer.parentElement ?? inputContainer;
		// eslint-disable-next-line no-restricted-syntax -- our own host element, created below on first use
		let host = parent.querySelector<HTMLElement>(':scope > .tauri-approvals-host');
		if (!host) {
			host = dom.$('.tauri-approvals-host');
			parent.insertBefore(host, inputContainer);
		}
		const cardHost = host;

		const card = new ApprovalCard(
			cardHost,
			entry.req,
			this.markdownRendererService,
			choice => this.settle(entry, choice),
		);
		entry.card = card;
	}

	/** Moves every request currently shown as a card to the modal dialog. */
	private rehomePendingCardsToModal(): void {
		// Copy first: settling mutates `_pending`.
		const carded = this._pending.filter(p => p.card && !p.settled);
		for (const entry of carded) {
			entry.card?.dispose();
			entry.card = undefined;
			this.showModal(entry);
		}
	}

	/**
	 * Fallback modal, matching the dialog `tauriMcpWriteTools.confirm()` showed. For high risk
	 * requests, Allow without the checkbox returns `'deny'`.
	 */
	private async showModal(entry: IPendingApproval): Promise<void> {
		if (entry.settled) {
			return;
		}
		const { client, tool, level, title, detail, highRisk } = entry.req;

		type Choice = ApprovalChoice | 'unchecked';
		const buttons: { label: string; run: (c: { checkboxChecked?: boolean }) => Choice }[] = [{
			label: highRisk ? localize('tauriApprovalsRunAnyway', "Run Anyway") : localize('tauriApprovalsAllow', "Allow"),
			run: c => !highRisk || c.checkboxChecked ? 'once' : 'unchecked',
		}];
		if (level === 2) {
			buttons.push({ label: localize('tauriApprovalsAllowSession', "Allow for This Session"), run: () => 'session' });
		}

		const { result } = await this.dialogService.prompt<Choice>({
			type: level === 3 ? Severity.Warning : Severity.Info,
			message: title,
			buttons,
			cancelButton: { label: localize('tauriApprovalsDeny', "Deny"), run: () => 'deny' },
			checkbox: highRisk ? { label: localize('tauriApprovalsUnderstand', "I understand the risk and want to run it"), checked: false } : undefined,
			custom: {
				markdownDetails: [
					{ markdown: new MarkdownString(localize('tauriApprovalsRequestedBy', "Requested by MCP client: **{0}** · tool `{1}`", client, tool)) },
					...(detail ? [{ markdown: detail }] : []),
				]
			},
		});

		// 'unchecked' (high risk, checkbox not ticked) and the cancel/deny path both deny.
		this.settle(entry, result === 'once' || result === 'session' ? result : 'deny');
	}
}

/**
 * A single approval card: client + tool header, title, rendered detail, and the Allow /
 * Allow for This Session / Deny buttons (plus the high-risk checkbox). Resolving disposes it.
 */
class ApprovalCard extends Disposable {

	private readonly element: HTMLElement;

	constructor(
		host: HTMLElement,
		req: IApprovalRequest,
		markdownRendererService: IMarkdownRendererService,
		private readonly onChoice: (choice: ApprovalChoice) => void,
	) {
		super();

		this.element = dom.append(host, dom.$('.tauri-approval-card'));
		if (req.highRisk) {
			this.element.classList.add('high-risk');
		}
		this.element.setAttribute('role', 'group');

		// Header: shield codicon + client name + tool.
		const header = dom.append(this.element, dom.$('.tauri-approval-header'));
		const icon = dom.append(header, dom.$('span.tauri-approval-icon'));
		icon.classList.add(...ThemeIcon.asClassNameArray(Codicon.shield));
		const headerText = dom.append(header, dom.$('span.tauri-approval-header-text'));
		headerText.textContent = localize('tauriApprovalsHeader', "{0} · {1}", req.client, req.tool);

		// Title (one line).
		const titleEl = dom.append(this.element, dom.$('.tauri-approval-title'));
		titleEl.textContent = req.title;

		// Rendered markdown detail. Not trusted: no command links, HTML is sanitized.
		if (req.detail) {
			const detailHost = dom.append(this.element, dom.$('.tauri-approval-detail'));
			const rendered = this._register(markdownRendererService.render(req.detail));
			detailHost.appendChild(rendered.element);
		}

		// High risk: a checkbox that must be ticked before Allow is enabled.
		let checkbox: Checkbox | undefined;
		if (req.highRisk) {
			const row = dom.append(this.element, dom.$('.tauri-approval-checkbox-row'));
			checkbox = this._register(new Checkbox(localize('tauriApprovalsUnderstand', "I understand the risk and want to run it"), false, defaultCheckboxStyles));
			dom.append(row, checkbox.domNode);
			const label = dom.append(row, dom.$('span.tauri-approval-checkbox-label'));
			label.textContent = localize('tauriApprovalsUnderstand', "I understand the risk and want to run it");
			this._register(dom.addDisposableListener(label, dom.EventType.CLICK, () => {
				checkbox!.checked = !checkbox!.checked;
				checkbox!.focus();
				updateAllowEnabled();
			}));
		}

		// Buttons.
		const actions = dom.append(this.element, dom.$('.tauri-approval-actions'));

		const allowButton = this._register(new Button(actions, defaultButtonStyles));
		allowButton.label = req.highRisk ? localize('tauriApprovalsRunAnyway', "Run Anyway") : localize('tauriApprovalsAllow', "Allow");
		this._register(allowButton.onDidClick(() => {
			if (req.highRisk && !(checkbox?.checked === true)) {
				return;
			}
			this.onChoice('once');
		}));

		if (req.level === 2) {
			const sessionButton = this._register(new Button(actions, { ...defaultButtonStyles, secondary: true }));
			sessionButton.label = localize('tauriApprovalsAllowSession', "Allow for This Session");
			this._register(sessionButton.onDidClick(() => this.onChoice('session')));
		}

		const denyButton = this._register(new Button(actions, { ...defaultButtonStyles, secondary: true }));
		denyButton.label = localize('tauriApprovalsDeny', "Deny");
		this._register(denyButton.onDidClick(() => this.onChoice('deny')));

		const updateAllowEnabled = () => {
			allowButton.enabled = !req.highRisk || checkbox?.checked === true;
		};
		if (checkbox) {
			this._register(checkbox.onChange(() => updateAllowEnabled()));
		}
		updateAllowEnabled();
	}

	override dispose(): void {
		this.element.remove();
		super.dispose();
	}
}

registerSingleton(ITauriApprovalService, TauriApprovalService, InstantiationType.Delayed);
