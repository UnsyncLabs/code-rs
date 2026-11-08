/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import { DeferredPromise } from '../../../../base/common/async.js';
import { Emitter, Event } from '../../../../base/common/event.js';
import { Disposable } from '../../../../base/common/lifecycle.js';
import type * as performance from '../../../../base/common/performance.js';
import { IProcessEnvironment, OperatingSystem, OS } from '../../../../base/common/platform.js';
import { TitleEventSource } from '../../../../platform/terminal/common/terminal.js';
import {
	IPtyHostLatencyMeasurement,
	IShellLaunchConfig,
	ITerminalBackend,
	ITerminalChildProcess,
	ITerminalProcessOptions,
	ITerminalProfile,
	ITerminalsLayoutInfo,
	ITerminalsLayoutInfoById,
	TerminalIcon,
} from '../../../../platform/terminal/common/terminal.js';
import { IProcessDetails } from '../../../../platform/terminal/common/terminalProcess.js';
import { createTauriPty } from '../../../../platform/terminal/browser/tauriPty.js';

/**
 * An {@link ITerminalBackend} that spawns terminals through the Rust `pty`
 * service in the Tauri shell (via {@link createTauriPty} /
 * `src/vs/platform/terminal/browser/tauriPty.ts`).
 *
 * This backend is intentionally *non-persistent*: the Rust pty host lives
 * in-process with the shell, so there is no reconnection/revive story and no
 * server-side layout to restore. Most of the `ITerminalBackend` surface that
 * exists purely for the remote/pty-host persistence model is therefore a safe
 * no-op here. The parts that matter — {@link createProcess},
 * {@link getDefaultSystemShell} and {@link getEnvironment} — are wired to the
 * native bridge.
 *
 * It does **not** extend `BaseTerminalBackend` because that base class is built
 * around an `IPtyHostController` (unresponsive-host status bar, resolve-variable
 * round-trips, serialized state revival) which the in-process Tauri pty has no
 * equivalent for.
 */
export class TauriTerminalBackend extends Disposable implements ITerminalBackend {

	readonly remoteAuthority = undefined;

	//#region Readiness

	private readonly _whenReady = new DeferredPromise<void>();
	get whenReady(): Promise<void> { return this._whenReady.p; }
	setReady(): void { this._whenReady.complete(); }

	//#endregion

	//#region Responsiveness (always responsive: the pty host is in-process)

	readonly isResponsive = true;

	readonly onPtyHostUnresponsive = Event.None;
	readonly onPtyHostResponsive = Event.None;
	readonly onPtyHostRestart = Event.None;

	//#endregion

	private readonly _onDidRequestDetach = this._register(new Emitter<{ requestId: number; workspaceId: string; instanceId: number }>());
	readonly onDidRequestDetach = this._onDidRequestDetach.event;

	constructor() {
		super();
		// Nothing to connect to: the Rust pty host is in-process and ready
		// immediately.
		this.setReady();
	}

	async createProcess(
		shellLaunchConfig: IShellLaunchConfig,
		cwd: string,
		cols: number,
		rows: number,
		_unicodeVersion: '6' | '11',
		env: IProcessEnvironment,
		_options: ITerminalProcessOptions,
		_shouldPersist: boolean
	): Promise<ITerminalChildProcess> {
		const shell = shellLaunchConfig.executable || await this.getDefaultSystemShell();
		const args = toArgArray(shellLaunchConfig.args);
		// Merge the process env with any per-launch overrides.
		const mergedEnv: { [key: string]: string | null | undefined } = { ...env, ...shellLaunchConfig.env };
		return createTauriPty(shell, args, cwd, mergedEnv, cols, rows);
	}

	//#region Persistence / reconnection — not supported in-process

	async attachToProcess(_id: number): Promise<ITerminalChildProcess | undefined> {
		return undefined;
	}

	async attachToRevivedProcess(_id: number, _pid: number): Promise<ITerminalChildProcess | undefined> {
		return undefined;
	}

	async listProcesses(): Promise<IProcessDetails[]> {
		return [];
	}

	async persistTerminalState(): Promise<void> {
		// No server-side state to persist.
	}

	async requestDetachInstance(_workspaceId: string, _instanceId: number): Promise<IProcessDetails | undefined> {
		return undefined;
	}

	async acceptDetachInstanceReply(_requestId: number, _persistentProcessId?: number): Promise<void> {
		// No-op: detach/attach is unsupported.
	}

	async reduceConnectionGraceTime(): Promise<void> {
		// No reconnection grace period for an in-process host.
	}

	restartPtyHost(): void {
		// The pty host cannot be restarted independently of the shell.
	}

	//#endregion

	//#region Environment / shells

	async getDefaultSystemShell(osOverride: OperatingSystem = OS): Promise<string> {
		if (osOverride === OperatingSystem.Windows) {
			return readProcessEnv('COMSPEC') ?? 'C:\\Windows\\System32\\cmd.exe';
		}
		return readProcessEnv('SHELL') ?? '/bin/bash';
	}

	async getProfiles(_profiles: unknown, _defaultProfile: unknown, _includeDetectedProfiles?: boolean): Promise<ITerminalProfile[]> {
		// Profile detection is not yet implemented natively; the default shell
		// path (above) is used when no profile is provided.
		return [];
	}

	async getEnvironment(): Promise<IProcessEnvironment> {
		// In the browser/webview we do not have direct access to the OS
		// environment; the Rust pty inherits the shell's environment on spawn.
		return {};
	}

	async getShellEnvironment(): Promise<IProcessEnvironment | undefined> {
		return undefined;
	}

	async getWslPath(original: string, _direction: 'unix-to-win' | 'win-to-unix'): Promise<string> {
		// No WSL path translation available without the Node pty host.
		return original;
	}

	//#endregion

	//#region Layout — kept in-memory only

	private _layoutInfo: ITerminalsLayoutInfoById | undefined;

	async setTerminalLayoutInfo(layoutInfo?: ITerminalsLayoutInfoById): Promise<void> {
		this._layoutInfo = layoutInfo;
	}

	async getTerminalLayoutInfo(): Promise<ITerminalsLayoutInfo | undefined> {
		// The in-process backend does not revive processes, so there is no
		// layout to hand back on reload.
		return undefined;
	}

	//#endregion

	//#region Misc no-ops / diagnostics

	async getLatency(): Promise<IPtyHostLatencyMeasurement[]> {
		return [];
	}

	async getPerformanceMarks(): Promise<performance.PerformanceMark[]> {
		return [];
	}

	async updateTitle(_id: number, _title: string, _titleSource: TitleEventSource): Promise<void> { }

	async updateIcon(_id: number, _userInitiated: boolean, _icon: TerminalIcon, _color?: string): Promise<void> { }

	async setNextCommandId(_id: number, _commandLine: string, _commandId: string): Promise<void> { }

	async installAutoReply(_match: string, _reply: string, _ownerId: string): Promise<void> { }

	async uninstallAllAutoReplies(_ownerId: string): Promise<void> { }

	//#endregion
}

function toArgArray(args: string | string[] | undefined): string[] {
	if (!args) {
		return [];
	}
	return Array.isArray(args) ? args : [args];
}

function readProcessEnv(key: string): string | undefined {
	// Best-effort read of a process env var if a shim exposes it on the global
	// (the webview itself has no `process`). Returns undefined otherwise so the
	// caller falls back to a platform default.
	const proc = (globalThis as unknown as { process?: { env?: Record<string, string | undefined> } }).process;
	return proc?.env?.[key];
}
