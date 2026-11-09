/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import { MarkdownString } from '../../../../base/common/htmlContent.js';
import { Disposable, toDisposable } from '../../../../base/common/lifecycle.js';
import Severity from '../../../../base/common/severity.js';
import { localize } from '../../../../nls.js';
import { ConfigurationScope, Extensions as ConfigurationExtensions, IConfigurationRegistry } from '../../../../platform/configuration/common/configurationRegistry.js';
import { IConfigurationService } from '../../../../platform/configuration/common/configuration.js';
import { IDialogService } from '../../../../platform/dialogs/common/dialogs.js';
import { ILogService } from '../../../../platform/log/common/log.js';
import { Registry } from '../../../../platform/registry/common/platform.js';
import { isTauri, ITauriExtensionAuditReport, setTauriExtensionReviewHandler, TauriAuditSeverity } from '../../../../platform/tauri/common/tauri.js';
import { IWorkbenchContribution } from '../../../common/contributions.js';
import { INotificationService } from '../../../../platform/notification/common/notification.js';
import { reviewExtensionReport } from './tauriAssistant.js';

/**
 * Extension security review (design: docs/rust-rewrite/MCP_DESIGN.md §6).
 *
 * Every gallery install or update is quarantined and audited by Rust; this contribution
 * shows the report and records the user's decision:
 *  - new installs always show the report;
 *  - updates show it when they raise the risk (medium/high findings or new capabilities);
 *  - `block-high` makes "Cancel" the default for high-risk packages.
 */

export const AUDIT_MODE_SETTING = 'codeRs.extensions.audit.mode';
type AuditMode = 'warn' | 'block-high' | 'off';

if (isTauri()) {
	Registry.as<IConfigurationRegistry>(ConfigurationExtensions.Configuration).registerConfiguration({
		id: 'codeRsExtensionAudit',
		order: 17,
		title: localize('codeRsAuditTitle', "Extension Security"),
		type: 'object',
		properties: {
			[AUDIT_MODE_SETTING]: {
				type: 'string',
				enum: ['warn', 'block-high', 'off'],
				enumDescriptions: [
					localize('codeRsAuditWarn', "Show the security report before installing an extension or a riskier update, and let me decide."),
					localize('codeRsAuditBlockHigh', "Like 'warn', but high-risk packages require an explicit 'install anyway'."),
					localize('codeRsAuditOff', "Do not show reports (not recommended). Packages are still quarantined and audited."),
				],
				default: 'warn',
				scope: ConfigurationScope.APPLICATION,
				markdownDescription: localize('codeRsAuditMode', "Extensions are downloaded to a quarantine folder and analyzed before they are installed: origin, SHA-256, publisher, native binaries, scripts, use of `child_process` / `eval`, network domains, obfuscation, and what changed since the installed version."),
			}
		}
	});
}

const SEVERITY_ICON: Record<TauriAuditSeverity, string> = { info: '$(info)', low: '$(pass)', medium: '$(warning)', high: '$(error)' };
const SEVERITY_ORDER: TauriAuditSeverity[] = ['high', 'medium', 'low', 'info'];

function riskLabel(risk: TauriAuditSeverity): string {
	switch (risk) {
		case 'high': return localize('codeRsRiskHigh', "HIGH");
		case 'medium': return localize('codeRsRiskMedium', "MEDIUM");
		default: return localize('codeRsRiskLow', "LOW");
	}
}

/** Escapes text coming from the package so it cannot inject markdown or links. */
function text(value: string): string {
	return new MarkdownString().appendText(value).value;
}

export function renderAuditReport(report: ITauriExtensionAuditReport): MarkdownString {
	const md = new MarkdownString('', { supportThemeIcons: true });
	const name = report.displayName ? `${report.displayName} (${report.id})` : report.id;
	md.appendMarkdown(`**${text(name)}** ${text(report.version)} — ${localize('codeRsRisk', "risk")}: **${riskLabel(report.risk)}**\n\n`);

	const findings = [...report.findings].sort((a, b) => SEVERITY_ORDER.indexOf(a.severity) - SEVERITY_ORDER.indexOf(b.severity));
	for (const f of findings) {
		md.appendMarkdown(`${SEVERITY_ICON[f.severity]} **${text(f.title)}** — ${text(f.detail)}\n\n`);
	}

	const p = report.publisher;
	md.appendMarkdown(`---\n\n`);
	md.appendMarkdown(`${localize('codeRsPublisher', "Publisher")}: \`${text(p.namespace)}\`${p.verified === true ? ' ✔' : ''}${p.publishedBy ? ` · ${localize('codeRsPublishedBy', "published by")} \`${text(p.publishedBy)}\`` : ''}${p.downloadCount !== undefined ? ` · ${p.downloadCount.toLocaleString()} ${localize('codeRsDownloads', "downloads")}` : ''}\n\n`);
	md.appendMarkdown(`${localize('codeRsPackage', "Package")}: ${(report.packageSize / 1024 / 1024).toFixed(1)} MB, ${report.fileCount} ${localize('codeRsFiles', "files")} · SHA-256 \`${text(report.sha256.slice(0, 16))}…\`\n\n`);
	if (report.domains.length) {
		md.appendMarkdown(`${localize('codeRsDomains', "Domains in code")}: ${report.domains.slice(0, 12).map(d => `\`${text(d)}\``).join(', ')}${report.domains.length > 12 ? ' …' : ''}\n\n`);
	}
	if (report.diff) {
		const d = report.diff;
		md.appendMarkdown(`${localize('codeRsDiff', "Changes since {0}", text(d.previousVersion))}: +${d.addedFiles} / −${d.removedFiles} ${localize('codeRsFiles', "files")}${d.newCapabilities.length ? `, ${localize('codeRsNewCaps', "new")}: ${text(d.newCapabilities.join(', '))}` : ''}\n\n`);
	}
	md.appendMarkdown(`*${text(report.disclaimer)}*`);
	return md;
}

export class TauriExtensionAuditContribution extends Disposable implements IWorkbenchContribution {

	static readonly ID = 'workbench.contrib.tauriExtensionAudit';

	constructor(
		@IDialogService private readonly dialogService: IDialogService,
		@IConfigurationService private readonly configurationService: IConfigurationService,
		@ILogService private readonly logService: ILogService,
		@INotificationService private readonly notificationService: INotificationService,
	) {
		super();
		if (!isTauri()) {
			return;
		}
		setTauriExtensionReviewHandler((report, isUpdate) => this.review(report, isUpdate));
		this._register(toDisposable(() => setTauriExtensionReviewHandler(undefined)));
	}

	private async review(report: ITauriExtensionAuditReport, isUpdate: boolean): Promise<boolean> {
		const mode = this.configurationService.getValue<AuditMode>(AUDIT_MODE_SETTING) ?? 'warn';
		this.logService.info(`[extension audit] ${report.id}@${report.version}: risk ${report.risk}, ${report.findings.length} findings, sha256 ${report.sha256}`);
		if (mode === 'off') {
			return true;
		}
		const riskierUpdate = report.risk === 'high' || report.risk === 'medium' || !!report.diff?.newCapabilities.length || !!report.diff?.newNativeBinaries.length || !!report.diff?.publisherChanged;
		if (isUpdate && !riskierUpdate) {
			return true;
		}

		const blockHigh = mode === 'block-high' && report.risk === 'high';
		const installLabel = blockHigh
			? localize('codeRsInstallAnyway', "Install Anyway (Not Recommended)")
			: isUpdate ? localize('codeRsUpdate', "Update") : localize('codeRsInstall', "Install");
		const details = [{ markdown: renderAuditReport(report) }];
		const ai = await this.aiOpinion(report);
		if (ai) {
			details.push({ markdown: ai });
		}
		const { result } = await this.dialogService.prompt<boolean>({
			type: report.risk === 'high' ? Severity.Error : report.risk === 'medium' ? Severity.Warning : Severity.Info,
			message: isUpdate
				? localize('codeRsReviewUpdate', "Review the update of '{0}' before installing", report.displayName ?? report.id)
				: localize('codeRsReviewInstall', "Review '{0}' before installing", report.displayName ?? report.id),
			buttons: [{ label: installLabel, run: () => true }],
			cancelButton: { label: localize('codeRsCancel', "Cancel"), run: () => false },
			custom: { markdownDetails: details },
		});
		return result === true;
	}

	/** Opinion of the configured AI reviewer, if any. Failures never block the install dialog. */
	private async aiOpinion(report: ITauriExtensionAuditReport): Promise<MarkdownString | undefined> {
		const status = this.notificationService.status(localize('codeRsAiReviewing', "AI is reviewing '{0}'...", report.displayName ?? report.id));
		try {
			const r = await reviewExtensionReport(this.configurationService, report);
			if (!r) {
				return undefined;
			}
			const md = new MarkdownString(`**${localize('codeRsAiOpinion', "AI review")}** (${r.provider} · ${r.model})

`);
			md.appendText(r.text);
			return md;
		} catch (error) {
			this.logService.warn(`[extension audit] AI review failed: ${error}`);
			return new MarkdownString().appendText(localize('codeRsAiFailed', "AI review unavailable: {0}", String(error)));
		} finally {
			status.close();
		}
	}
}
