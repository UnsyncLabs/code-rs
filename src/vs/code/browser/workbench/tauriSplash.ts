/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

/* eslint-disable no-restricted-globals, no-restricted-syntax */

// code-rs (Tauri): paints the same "parts splash" skeleton the desktop (Electron)
// build shows while a window (re)loads, so opening/closing folders keeps the
// layout on screen instead of flashing an empty page. The workbench stores the
// splash data in localStorage ('monaco-parts-splash', see splash.contribution.ts)
// and removes the skeleton once it has rendered (see partsSplash.ts).
// The drawing code below is shared verbatim with electron-browser/workbench/workbench.ts.

import { getPartsSplashColors, getPartsSplashLayoutMetrics } from './partsSplash.js';
import type { IPartsSplash, IPartsSplashPartBounds } from '../../../platform/theme/common/themeService.js';
import { isTauri } from '../../../platform/tauri/common/tauri.js';

interface ISplashConfiguration {
	partsSplash?: IPartsSplash;
	workspace?: unknown;
	autoDetectHighContrast?: boolean;
	autoDetectColorScheme?: boolean;
	colorScheme: { dark: boolean; highContrast: boolean };
	extensionDevelopmentPath?: string[];
}

const preloadGlobals: { webFrame?: { setZoomLevel?(level: number): void } } | undefined = undefined;

(function () {
	if (!isTauri() || document.getElementById('monaco-parts-splash')) {
		return;
	}

	let partsSplash: IPartsSplash | undefined;
	try {
		const raw = localStorage.getItem('monaco-parts-splash');
		partsSplash = raw ? JSON.parse(raw) : undefined;
	} catch {
		partsSplash = undefined;
	}

	const query = new URL(document.location.href).searchParams;
	const hasWorkspace = query.has('folder') || query.has('workspace') || query.has('tauriFolder');

	showDefaultSplash({
		partsSplash,
		workspace: hasWorkspace ? {} : undefined,
		colorScheme: { dark: false, highContrast: false },
	});
	function showDefaultSplash(configuration: ISplashConfiguration) {
		let data = configuration.partsSplash;
		if (data) {
			if (configuration.autoDetectHighContrast && configuration.colorScheme.highContrast) {
				if ((configuration.colorScheme.dark && data.baseTheme !== 'hc-black') || (!configuration.colorScheme.dark && data.baseTheme !== 'hc-light')) {
					data = undefined; // high contrast mode has been turned by the OS -> ignore stored colors and layouts
				}
			} else if (configuration.autoDetectColorScheme) {
				if ((configuration.colorScheme.dark && data.baseTheme !== 'vs-dark') || (!configuration.colorScheme.dark && data.baseTheme !== 'vs')) {
					data = undefined; // OS color scheme is tracked and has changed
				}
			}
		}

		// developing an extension -> ignore stored layouts
		if (data && configuration.extensionDevelopmentPath) {
			data.layoutInfo = undefined;
		}

		// minimal color configuration (works with or without persisted data)
		let baseTheme;
		let shellBackground;
		let shellForeground;
		const splashColors = data ? getPartsSplashColors(data, window.document.hasFocus(), !!configuration.workspace) : undefined;
		if (data) {
			baseTheme = data.baseTheme;
			shellBackground = splashColors?.background;
			shellForeground = data.colorInfo.foreground;
		} else if (configuration.autoDetectHighContrast && configuration.colorScheme.highContrast) {
			if (configuration.colorScheme.dark) {
				baseTheme = 'hc-black';
				shellBackground = '#000000';
				shellForeground = '#FFFFFF';
			} else {
				baseTheme = 'hc-light';
				shellBackground = '#FFFFFF';
				shellForeground = '#000000';
			}
		} else if (configuration.autoDetectColorScheme) {
			if (configuration.colorScheme.dark) {
				baseTheme = 'vs-dark';
				shellBackground = '#1E1E1E';
				shellForeground = '#CCCCCC';
			} else {
				baseTheme = 'vs';
				shellBackground = '#FFFFFF';
				shellForeground = '#000000';
			}
		}

		const style = document.createElement('style');
		style.className = 'initialShellColors';
		window.document.head.appendChild(style);
		style.textContent = `body {	background-color: ${shellBackground}; color: ${shellForeground}; margin: 0; padding: 0; }`;

		// set zoom level as soon as possible
		if (typeof data?.zoomLevel === 'number' && typeof preloadGlobals?.webFrame?.setZoomLevel === 'function') {
			preloadGlobals.webFrame.setZoomLevel(data.zoomLevel);
		}

		// restore parts if possible (we might not always store layout info)
		if (data?.layoutInfo) {
			const { layoutInfo, colorInfo } = data;
			const modernUI = layoutInfo.modernUI === true;
			const { floatingMargin, floatingOuterMargin, floatingBorderWidth, floatingBorderRadius } = getPartsSplashLayoutMetrics(layoutInfo);
			const contentTop = layoutInfo.titleBarHeight;
			const contentBottom = layoutInfo.statusBarHeight;

			const splash = document.createElement('div');
			splash.id = 'monaco-parts-splash';
			splash.className = baseTheme ?? 'vs-dark';

			if (layoutInfo.windowBorder && colorInfo.windowBorder) {
				const borderElement = document.createElement('div');
				borderElement.style.position = 'absolute';
				borderElement.style.width = 'calc(100vw - 2px)';
				borderElement.style.height = 'calc(100vh - 2px)';
				borderElement.style.zIndex = '1'; // allow border above other elements
				borderElement.style.border = `1px solid var(--window-border-color)`;
				borderElement.style.setProperty('--window-border-color', colorInfo.windowBorder);

				if (layoutInfo.windowBorderRadius) {
					borderElement.style.borderRadius = layoutInfo.windowBorderRadius;
				}

				splash.appendChild(borderElement);
			}

			const setBounds = (element: HTMLElement, bounds: { top: number; bottom?: number; left?: number; right?: number; width?: number; height?: number }) => {
				element.style.position = 'absolute';
				element.style.top = `${bounds.top}px`;
				if (typeof bounds.bottom === 'number') {
					element.style.bottom = `${bounds.bottom}px`;
				}
				if (typeof bounds.left === 'number') {
					element.style.left = `${bounds.left}px`;
				}
				if (typeof bounds.right === 'number') {
					element.style.right = `${bounds.right}px`;
				}
				if (typeof bounds.width === 'number') {
					element.style.width = `${bounds.width}px`;
				}
				if (typeof bounds.height === 'number') {
					element.style.height = `${bounds.height}px`;
				}
			};

			const setPartBounds = (element: HTMLElement, bounds: { top: number; left: number; width: number; height: number }) => {
				element.style.position = 'absolute';
				element.style.top = `${bounds.top}px`;
				element.style.left = `${bounds.left}px`;
				element.style.width = `${bounds.width}px`;
				element.style.height = `${bounds.height}px`;
			};

			const fallbackActivityBarBounds: IPartsSplashPartBounds | undefined = layoutInfo.modernUICompact === true && layoutInfo.activityBarWidth > 0 ? {
				top: contentTop + (contentTop === 0 ? floatingOuterMargin : 0),
				left: layoutInfo.sideBarSide === 'left' ? floatingOuterMargin : window.innerWidth - layoutInfo.activityBarWidth,
				width: Math.max(0, layoutInfo.activityBarWidth - floatingOuterMargin),
				height: window.innerHeight - contentTop - contentBottom - floatingOuterMargin - (contentTop === 0 ? floatingOuterMargin : 0),
			} : undefined;
			const compactPartBounds = layoutInfo.modernUICompact === true ? [
				layoutInfo.partBounds?.activityBar,
				layoutInfo.partBounds?.sideBar,
				layoutInfo.partBounds?.auxiliaryBar,
				layoutInfo.partBounds?.editor,
				layoutInfo.partBounds?.panel,
			].filter((bounds): bounds is IPartsSplashPartBounds => !!bounds) : [];
			const compactHorizontalBounds = layoutInfo.modernUICompact === true ? [
				layoutInfo.partBounds?.activityBar ?? fallbackActivityBarBounds,
				layoutInfo.partBounds?.sideBar,
				layoutInfo.partBounds?.auxiliaryBar,
				layoutInfo.partBounds?.editor,
				layoutInfo.partBounds?.panel,
			].filter((bounds): bounds is IPartsSplashPartBounds => !!bounds) : [];
			const compactClusterEdges = compactPartBounds.length > 0 && compactHorizontalBounds.length > 0 ? {
				left: Math.min(...compactHorizontalBounds.map(bounds => bounds.left)),
				right: Math.max(...compactHorizontalBounds.map(bounds => bounds.left + bounds.width)),
				top: Math.min(...compactPartBounds.map(bounds => bounds.top)),
				bottom: Math.max(...compactPartBounds.map(bounds => bounds.top + bounds.height)),
			} : undefined;

			// Without saved `partBounds` (they are cleared whenever the resolved bar widths differ
			// from the ones stored for the workspace) the cluster edges cannot be measured, so
			// derive ownership from the fallback order instead: the outermost visible card on each
			// side owns that edge, and every card spans the content region vertically.
			const fallbackClusterOrder: readonly ('activityBar' | 'sideBar' | 'editor' | 'auxiliaryBar')[] = layoutInfo.sideBarSide === 'left'
				? ['activityBar', 'sideBar', 'editor', 'auxiliaryBar']
				: ['auxiliaryBar', 'editor', 'sideBar', 'activityBar'];
			const fallbackClusterVisible = {
				activityBar: layoutInfo.activityBarWidth > 0,
				sideBar: layoutInfo.sideBarWidth > 0,
				editor: true,
				auxiliaryBar: layoutInfo.auxiliaryBarWidth > 0,
			};
			const fallbackLeftOwner = fallbackClusterOrder.find(part => fallbackClusterVisible[part]);
			const fallbackRightOwner = [...fallbackClusterOrder].reverse().find(part => fallbackClusterVisible[part]);
			const fallbackOuterEdgesFor = (part: 'activityBar' | 'sideBar' | 'editor' | 'auxiliaryBar') => ({
				left: fallbackLeftOwner === part,
				right: fallbackRightOwner === part,
				top: true,
				bottom: true,
			});
			const fallbackInsetFor = (part: 'activityBar' | 'sideBar' | 'editor' | 'auxiliaryBar', edge: 'left' | 'right' | 'top' | 'bottom') =>
				modernUI ? fallbackOuterEdgesFor(part)[edge] ? floatingOuterMargin : floatingMargin : 0;

			const railBorderColor = colorInfo.modernActivityBarBorder ?? colorInfo.surfaceBorder ?? colorInfo.agentsPanelBorder ?? colorInfo.editorGroupBorder ?? 'transparent';

			const applyFloatingCardStyles = (
				element: HTMLElement,
				backgroundColor: string | undefined,
				partBounds?: IPartsSplashPartBounds,
				fallbackOuterEdges = { left: true, right: true, top: true, bottom: true },
				compactBorderColor = colorInfo.surfaceBorder ?? colorInfo.agentsPanelBorder ?? colorInfo.editorGroupBorder ?? 'transparent'
			) => {
				element.style.boxSizing = 'border-box';
				if (layoutInfo.modernUICompact === true) {
					const outerEdges = partBounds?.outerEdges ?? (partBounds && compactClusterEdges ? {
						left: partBounds.left === compactClusterEdges.left,
						right: partBounds.left + partBounds.width === compactClusterEdges.right,
						top: partBounds.top === compactClusterEdges.top,
						bottom: partBounds.top + partBounds.height === compactClusterEdges.bottom,
					} : fallbackOuterEdges);
					element.style.borderStyle = 'solid';
					element.style.borderColor = compactBorderColor;
					element.style.borderWidth = `${outerEdges.top ? floatingBorderWidth : 0}px ${floatingBorderWidth}px ${floatingBorderWidth}px ${outerEdges.left ? floatingBorderWidth : 0}px`;
					element.style.borderRadius = [
						outerEdges.top && outerEdges.left ? floatingBorderRadius : 0,
						outerEdges.top && outerEdges.right ? floatingBorderRadius : 0,
						outerEdges.bottom && outerEdges.right ? floatingBorderRadius : 0,
						outerEdges.bottom && outerEdges.left ? floatingBorderRadius : 0,
					].map(radius => `${radius}px`).join(' ');
				} else {
					element.style.border = `${floatingBorderWidth}px solid ${colorInfo.agentsPanelBorder ?? colorInfo.editorGroupBorder ?? 'transparent'}`;
					element.style.borderRadius = `${floatingBorderRadius}px`;
				}
				element.style.backgroundColor = backgroundColor ?? colorInfo.editorBackground ?? colorInfo.background;
				element.style.overflow = 'hidden';
			};

			const contentHeight = `calc(100% - ${contentTop + contentBottom}px)`;
			const activityHeight = modernUI ? `calc(100% - ${contentTop + contentBottom + floatingMargin}px)` : contentHeight;
			const modernActivityBarBackground = (window.document.hasFocus()
				? colorInfo.modernActivityBarBackground
				: colorInfo.modernActivityBarInactiveBackground ?? colorInfo.modernActivityBarBackground)
				?? colorInfo.activityBarBackground;

			if (layoutInfo.auxiliaryBarWidth === Number.MAX_SAFE_INTEGER) {
				// if auxiliary bar is maximized, it goes as wide as the
				// window width but leaving room for activity bar
				layoutInfo.auxiliaryBarWidth = window.innerWidth - layoutInfo.activityBarWidth;
			} else {
				// otherwise adjust for other parts sizes if not maximized
				layoutInfo.auxiliaryBarWidth = Math.min(layoutInfo.auxiliaryBarWidth, window.innerWidth - (layoutInfo.activityBarWidth + layoutInfo.editorPartMinWidth + layoutInfo.sideBarWidth));
			}
			layoutInfo.sideBarWidth = Math.min(layoutInfo.sideBarWidth, window.innerWidth - (layoutInfo.activityBarWidth + layoutInfo.editorPartMinWidth + layoutInfo.auxiliaryBarWidth));

			// part: title
			if (layoutInfo.titleBarHeight > 0) {
				const titleDiv = document.createElement('div');
				titleDiv.style.position = 'absolute';
				titleDiv.style.width = '100%';
				titleDiv.style.height = `${layoutInfo.titleBarHeight}px`;
				titleDiv.style.left = '0';
				titleDiv.style.top = '0';
				titleDiv.style.backgroundColor = splashColors?.titleBarBackground ?? '';
				(titleDiv.style as CSSStyleDeclaration & { '-webkit-app-region': string })['-webkit-app-region'] = 'drag';
				splash.appendChild(titleDiv);

				if (!modernUI && colorInfo.titleBarBorder) {
					const titleBorder = document.createElement('div');
					titleBorder.style.position = 'absolute';
					titleBorder.style.width = '100%';
					titleBorder.style.height = '1px';
					titleBorder.style.left = '0';
					titleBorder.style.bottom = '0';
					titleBorder.style.borderBottom = `1px solid ${colorInfo.titleBarBorder}`;
					titleDiv.appendChild(titleBorder);
				}
			}

			// part: activity bar
			if (layoutInfo.activityBarWidth > 0) {
				const activityDiv = document.createElement('div');
				const activityBarBounds = layoutInfo.partBounds?.activityBar;
				if (modernUI && activityBarBounds) {
					setPartBounds(activityDiv, activityBarBounds);
				} else if (layoutInfo.modernUICompact === true) {
					setBounds(activityDiv, {
						top: contentTop + (contentTop === 0 ? floatingOuterMargin : 0),
						bottom: contentBottom + floatingOuterMargin,
						...(layoutInfo.sideBarSide === 'left' ? { left: floatingOuterMargin } : { right: floatingOuterMargin }),
						width: Math.max(0, layoutInfo.activityBarWidth - floatingOuterMargin),
					});
				} else {
					activityDiv.style.position = 'absolute';
					activityDiv.style.width = `${layoutInfo.activityBarWidth}px`;
					activityDiv.style.height = activityHeight;
					activityDiv.style.top = `${contentTop}px`;
					if (layoutInfo.sideBarSide === 'left') {
						activityDiv.style.left = '0';
					} else {
						activityDiv.style.right = '0';
					}
				}
				if (layoutInfo.modernUICompact === true) {
					applyFloatingCardStyles(activityDiv, modernActivityBarBackground, activityBarBounds, fallbackOuterEdgesFor('activityBar'), railBorderColor);
				} else if (modernUI) {
					// The rail is a card here too: rounded on the window side, and square where it
					// meets the primary side bar so the two read as one connected surface.
					const radius = `${floatingBorderRadius}px`;
					activityDiv.style.boxSizing = 'border-box';
					activityDiv.style.backgroundColor = modernActivityBarBackground ?? 'transparent';
					activityDiv.style.border = `${floatingBorderWidth}px solid ${railBorderColor}`;
					activityDiv.style.borderRadius = layoutInfo.sideBarWidth === 0 ? radius
						: layoutInfo.sideBarSide === 'left' ? `${radius} 0 0 ${radius}` : `0 ${radius} ${radius} 0`;
					activityDiv.style.overflow = 'hidden';
				} else {
					activityDiv.style.backgroundColor = `${colorInfo.activityBarBackground}`;
				}
				splash.appendChild(activityDiv);

				if (!modernUI && colorInfo.activityBarBorder) {
					const activityBorderDiv = document.createElement('div');
					activityBorderDiv.style.position = 'absolute';
					activityBorderDiv.style.width = '1px';
					activityBorderDiv.style.height = '100%';
					activityBorderDiv.style.top = '0';
					if (layoutInfo.sideBarSide === 'left') {
						activityBorderDiv.style.right = '0';
						activityBorderDiv.style.borderRight = `1px solid ${colorInfo.activityBarBorder}`;
					} else {
						activityBorderDiv.style.left = '0';
						activityBorderDiv.style.borderLeft = `1px solid ${colorInfo.activityBarBorder}`;
					}
					activityDiv.appendChild(activityBorderDiv);
				}
			}

			// part: side bar
			if (layoutInfo.sideBarWidth > 0) {
				// The side bar meets the activity bar rail flush; with no rail it is the outermost
				// card on that edge and takes the cluster's outer gutter instead.
				const sideBarFallbackOuterEdges = fallbackOuterEdgesFor('sideBar');
				const sideBarClusterInset = modernUI && sideBarFallbackOuterEdges[layoutInfo.sideBarSide === 'left' ? 'left' : 'right'] ? floatingOuterMargin : 0;
				const sideDiv = document.createElement('div');
				if (modernUI && layoutInfo.partBounds?.sideBar) {
					setPartBounds(sideDiv, layoutInfo.partBounds.sideBar);
				} else if (layoutInfo.sideBarSide === 'left') {
					setBounds(sideDiv, {
						top: contentTop + (contentTop === 0 ? fallbackInsetFor('sideBar', 'top') : 0),
						bottom: contentBottom + fallbackInsetFor('sideBar', 'bottom'),
						left: layoutInfo.activityBarWidth + sideBarClusterInset,
						width: modernUI ? Math.max(0, layoutInfo.sideBarWidth - sideBarClusterInset - floatingBorderWidth * 2) : layoutInfo.sideBarWidth
					});
				} else {
					setBounds(sideDiv, {
						top: contentTop + (contentTop === 0 ? fallbackInsetFor('sideBar', 'top') : 0),
						bottom: contentBottom + fallbackInsetFor('sideBar', 'bottom'),
						right: layoutInfo.activityBarWidth + sideBarClusterInset,
						width: modernUI ? Math.max(0, layoutInfo.sideBarWidth - sideBarClusterInset - floatingBorderWidth * 2) : layoutInfo.sideBarWidth
					});
				}
				if (modernUI) {
					applyFloatingCardStyles(sideDiv, colorInfo.surfaceBackground ?? colorInfo.agentsPanelBackground ?? colorInfo.sideBarBackground, layoutInfo.partBounds?.sideBar, sideBarFallbackOuterEdges);
				} else {
					sideDiv.style.backgroundColor = `${colorInfo.sideBarBackground}`;
				}
				splash.appendChild(sideDiv);

				if (!modernUI && colorInfo.sideBarBorder) {
					const sideBorderDiv = document.createElement('div');
					sideBorderDiv.style.position = 'absolute';
					sideBorderDiv.style.width = '1px';
					sideBorderDiv.style.height = '100%';
					sideBorderDiv.style.top = '0';
					sideBorderDiv.style.right = '0';
					if (layoutInfo.sideBarSide === 'left') {
						sideBorderDiv.style.borderRight = `1px solid ${colorInfo.sideBarBorder}`;
					} else {
						sideBorderDiv.style.left = '0';
						sideBorderDiv.style.borderLeft = `1px solid ${colorInfo.sideBarBorder}`;
					}
					sideDiv.appendChild(sideBorderDiv);
				}
			}

			// part: auxiliary sidebar
			if (layoutInfo.auxiliaryBarWidth > 0) {
				const auxiliaryBarFallbackOuterEdges = fallbackOuterEdgesFor('auxiliaryBar');
				const auxSideDiv = document.createElement('div');
				if (modernUI && layoutInfo.partBounds?.auxiliaryBar) {
					setPartBounds(auxSideDiv, layoutInfo.partBounds.auxiliaryBar);
				} else if (layoutInfo.sideBarSide === 'left') {
					setBounds(auxSideDiv, {
						top: contentTop + (contentTop === 0 ? fallbackInsetFor('auxiliaryBar', 'top') : 0),
						bottom: contentBottom + fallbackInsetFor('auxiliaryBar', 'bottom'),
						right: fallbackInsetFor('auxiliaryBar', 'right'),
						width: modernUI ? Math.max(0, layoutInfo.auxiliaryBarWidth - fallbackInsetFor('auxiliaryBar', 'right') - floatingMargin - floatingBorderWidth * 2) : layoutInfo.auxiliaryBarWidth
					});
				} else {
					setBounds(auxSideDiv, {
						top: contentTop + (contentTop === 0 ? fallbackInsetFor('auxiliaryBar', 'top') : 0),
						bottom: contentBottom + fallbackInsetFor('auxiliaryBar', 'bottom'),
						left: fallbackInsetFor('auxiliaryBar', 'left'),
						width: modernUI ? Math.max(0, layoutInfo.auxiliaryBarWidth - fallbackInsetFor('auxiliaryBar', 'left') - floatingMargin - floatingBorderWidth * 2) : layoutInfo.auxiliaryBarWidth
					});
				}
				if (modernUI) {
					applyFloatingCardStyles(auxSideDiv, colorInfo.sideBarBackground, layoutInfo.partBounds?.auxiliaryBar, auxiliaryBarFallbackOuterEdges);
				} else {
					auxSideDiv.style.backgroundColor = `${colorInfo.sideBarBackground}`;
				}
				splash.appendChild(auxSideDiv);

				if (!modernUI && colorInfo.sideBarBorder) {
					const auxSideBorderDiv = document.createElement('div');
					auxSideBorderDiv.style.position = 'absolute';
					auxSideBorderDiv.style.width = '1px';
					auxSideBorderDiv.style.height = '100%';
					auxSideBorderDiv.style.top = '0';
					if (layoutInfo.sideBarSide === 'left') {
						auxSideBorderDiv.style.left = '0';
						auxSideBorderDiv.style.borderLeft = `1px solid ${colorInfo.sideBarBorder}`;
					} else {
						auxSideBorderDiv.style.right = '0';
						auxSideBorderDiv.style.borderRight = `1px solid ${colorInfo.sideBarBorder}`;
					}
					auxSideDiv.appendChild(auxSideBorderDiv);
				}
			}

			if (modernUI && (layoutInfo.partBounds?.editor || !layoutInfo.partBounds)) {
				const editorFallbackOuterEdges = fallbackOuterEdgesFor('editor');
				const editorDiv = document.createElement('div');
				if (layoutInfo.partBounds?.editor) {
					setPartBounds(editorDiv, layoutInfo.partBounds.editor);
				} else {
					const editorLeft = (layoutInfo.sideBarSide === 'left' ? layoutInfo.activityBarWidth + layoutInfo.sideBarWidth : layoutInfo.auxiliaryBarWidth) + fallbackInsetFor('editor', 'left');
					const editorRight = (layoutInfo.sideBarSide === 'left' ? layoutInfo.auxiliaryBarWidth : layoutInfo.activityBarWidth + layoutInfo.sideBarWidth) + fallbackInsetFor('editor', 'right');
					setBounds(editorDiv, {
						top: contentTop + (contentTop === 0 ? fallbackInsetFor('editor', 'top') : 0),
						bottom: contentBottom + fallbackInsetFor('editor', 'bottom'),
						left: editorLeft,
						right: editorRight
					});
				}
				applyFloatingCardStyles(editorDiv, colorInfo.editorBackground, layoutInfo.partBounds?.editor, editorFallbackOuterEdges, colorInfo.editorBorder ?? colorInfo.surfaceBorder ?? colorInfo.editorGroupBorder ?? 'transparent');
				splash.appendChild(editorDiv);
			}

			if (modernUI && layoutInfo.partBounds?.panel) {
				const panelDiv = document.createElement('div');
				setPartBounds(panelDiv, layoutInfo.partBounds.panel);
				applyFloatingCardStyles(panelDiv, colorInfo.panelBackground ?? colorInfo.editorBackground, layoutInfo.partBounds.panel);
				panelDiv.style.borderColor = colorInfo.modernPanelBorder ?? colorInfo.surfaceBorder ?? colorInfo.agentsPanelBorder ?? colorInfo.editorGroupBorder ?? 'transparent';
				splash.appendChild(panelDiv);
			}

			// part: statusbar
			if (layoutInfo.statusBarHeight > 0) {
				const statusDiv = document.createElement('div');
				statusDiv.style.position = 'absolute';
				statusDiv.style.width = '100%';
				statusDiv.style.height = `${layoutInfo.statusBarHeight}px`;
				statusDiv.style.bottom = '0';
				statusDiv.style.left = '0';
				statusDiv.style.backgroundColor = splashColors?.statusBarBackground ?? '';
				splash.appendChild(statusDiv);

				if (!modernUI && colorInfo.statusBarBorder) {
					const statusBorderDiv = document.createElement('div');
					statusBorderDiv.style.position = 'absolute';
					statusBorderDiv.style.width = '100%';
					statusBorderDiv.style.height = '1px';
					statusBorderDiv.style.top = '0';
					statusBorderDiv.style.borderTop = `1px solid ${colorInfo.statusBarBorder}`;
					statusDiv.appendChild(statusBorderDiv);
				}
			}

			window.document.body.appendChild(splash);
		}
	}
})();
