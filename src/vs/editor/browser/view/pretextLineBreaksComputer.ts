/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

import { CharCode } from '../../../base/common/charCode.js';
import * as strings from '../../../base/common/strings.js';
import { resolveAmdNodeModulePath } from '../../../amdX.js';
import { safeIntl } from '../../../base/common/date.js';
import { WrappingIndent } from '../../common/config/editorOptions.js';
import { FontInfo } from '../../common/config/fontInfo.js';
import { ILineBreaksComputer, ILineBreaksComputerContext, ILineBreaksComputerFactory, ModelLineProjectionData } from '../../common/modelLineProjectionData.js';
import { DOMLineBreaksComputerFactory } from './domLineBreaksComputer.js';

/**
 * EXPERIMENT (code-rs): line wrapping for `wrappingStrategy: 'advanced'` computed with
 * Pretext (https://github.com/chenglou/pretext) instead of laying lines out in a hidden
 * DOM node and probing `getClientRects()`.
 *
 * Pretext measures text segments once with canvas (cached) and breaks lines with plain
 * arithmetic, so no DOM layout/reflow happens. Lines with features Pretext does not model
 * (injected text, two-cell fullwidth rendering) are delegated to the DOM computer.
 */

type PretextModule = typeof import('@chenglou/pretext');

let pretext: PretextModule | undefined;
let pretextLoading: Promise<PretextModule | undefined> | undefined;

/** Loads Pretext from node_modules. Until it is loaded the DOM computer is used. */
export function loadPretext(): Promise<PretextModule | undefined> {
	// Pretext ships as an ES module, which `importAMDNodeModule` cannot load.
	// eslint-disable-next-line local/code-amd-node-module
	pretextLoading ??= import(/* webpackIgnore: true */ /* @vite-ignore */ resolveAmdNodeModulePath('@chenglou/pretext', 'dist/layout.js'))
		.then(module => (pretext = module as PretextModule))
		.catch(err => {
			console.error('[pretext] failed to load', err);
			return undefined;
		});
	return pretextLoading;
}

export function isPretextLoaded(): boolean {
	return !!pretext;
}

export class PretextLineBreaksComputerFactory implements ILineBreaksComputerFactory {

	public static create(targetWindow: Window): PretextLineBreaksComputerFactory {
		return new PretextLineBreaksComputerFactory(DOMLineBreaksComputerFactory.create(targetWindow));
	}

	constructor(private readonly domFallback: DOMLineBreaksComputerFactory) {
		loadPretext();
	}

	public createLineBreaksComputer(context: ILineBreaksComputerContext, fontInfo: FontInfo, tabSize: number, wrappingColumn: number, wrappingIndent: WrappingIndent, wordBreak: 'normal' | 'keepAll', wrapOnEscapedLineFeeds: boolean, useTwoCellFullwidthCharacters: boolean): ILineBreaksComputer {
		const lineNumbers: number[] = [];
		const previous: (ModelLineProjectionData | null)[] = [];
		return {
			addRequest: (lineNumber: number, previousLineBreakData: ModelLineProjectionData | null) => {
				lineNumbers.push(lineNumber);
				previous.push(previousLineBreakData);
			},
			finalize: () => {
				const module = pretext;
				const result: (ModelLineProjectionData | null)[] = new Array(lineNumbers.length);
				const fallbackIndices: number[] = [];

				if (!module || wrappingColumn === -1 || useTwoCellFullwidthCharacters) {
					for (let i = 0; i < lineNumbers.length; i++) {
						fallbackIndices.push(i);
					}
				} else {
					const font = toCanvasFont(fontInfo);
					const options = { whiteSpace: 'pre-wrap' as const, wordBreak: wordBreak === 'keepAll' ? 'keep-all' as const : 'normal' as const, letterSpacing: fontInfo.letterSpacing || undefined };
					for (let i = 0; i < lineNumbers.length; i++) {
						const lineNumber = lineNumbers[i];
						if (context.getLineInjectedText(lineNumber)) {
							fallbackIndices.push(i);
							continue;
						}
						const computed = computeLineBreaks(module, context.getLineContent(lineNumber), fontInfo, font, options, tabSize, wrappingColumn, wrappingIndent);
						if (computed === undefined) {
							fallbackIndices.push(i);
						} else {
							result[i] = computed;
						}
					}
				}

				if (fallbackIndices.length > 0) {
					const dom = this.domFallback.createLineBreaksComputer(context, fontInfo, tabSize, wrappingColumn, wrappingIndent, wordBreak, wrapOnEscapedLineFeeds, useTwoCellFullwidthCharacters);
					for (const i of fallbackIndices) {
						dom.addRequest(lineNumbers[i], previous[i]);
					}
					const domResult = dom.finalize();
					fallbackIndices.forEach((i, j) => result[i] = domResult[j]);
				}
				return result;
			}
		};
	}
}

function toCanvasFont(fontInfo: FontInfo): string {
	return `${fontInfo.fontWeight} ${fontInfo.fontSize}px ${fontInfo.fontFamily}`;
}

const graphemeSegmenter = safeIntl.Segmenter(undefined, { granularity: 'grapheme' });

/**
 * Returns the projection data for one line, `null` when it fits, or `undefined` when the
 * line must be handled by the DOM computer.
 */
function computeLineBreaks(
	module: PretextModule,
	lineContent: string,
	fontInfo: FontInfo,
	font: string,
	options: { whiteSpace: 'pre-wrap'; wordBreak: 'normal' | 'keep-all'; letterSpacing: number | undefined },
	tabSize: number,
	firstLineBreakColumn: number,
	wrappingIndent: WrappingIndent,
): ModelLineProjectionData | null | undefined {
	if (lineContent.length <= 1) {
		return null;
	}

	const overallWidth = Math.round(firstLineBreakColumn * fontInfo.typicalHalfwidthCharacterWidth);
	const additionalIndent = (wrappingIndent === WrappingIndent.DeepIndent ? 2 : wrappingIndent === WrappingIndent.Indent ? 1 : 0);
	const additionalIndentSize = Math.round(tabSize * additionalIndent);
	const additionalIndentLength = Math.ceil(fontInfo.spaceWidth * additionalIndentSize);

	// Same indentation tracking as the DOM computer.
	let firstNonWhitespaceIndex = 0;
	let wrappedTextIndentLength = 0;
	let width = overallWidth;
	if (wrappingIndent !== WrappingIndent.None) {
		firstNonWhitespaceIndex = strings.firstNonWhitespaceIndex(lineContent);
		if (firstNonWhitespaceIndex === -1) {
			firstNonWhitespaceIndex = 0;
		} else {
			for (let i = 0; i < firstNonWhitespaceIndex; i++) {
				wrappedTextIndentLength += lineContent.charCodeAt(i) === CharCode.Tab ? (tabSize - (wrappedTextIndentLength % tabSize)) : 1;
			}
			const indentWidth = Math.ceil(fontInfo.spaceWidth * wrappedTextIndentLength);
			if (indentWidth + fontInfo.typicalFullwidthCharacterWidth > overallWidth) {
				firstNonWhitespaceIndex = 0;
				wrappedTextIndentLength = 0;
			} else {
				width = overallWidth - indentWidth;
			}
		}
	}

	// Render the line the way the DOM computer does (tabs expanded, control characters
	// replaced), remembering where each model character starts in the rendered text.
	const renderLineContent = lineContent.substring(firstNonWhitespaceIndex);
	const len = renderLineContent.length;
	const charOffsets = new Array<number>(len + 1);
	const visibleColumns = new Array<number>(len + 1);
	let rendered = '';
	let visibleColumn = wrappedTextIndentLength;
	let charOffset = 0;
	for (let charIndex = 0; charIndex < len; charIndex++) {
		charOffsets[charIndex] = charOffset;
		visibleColumns[charIndex] = visibleColumn;
		const charCode = renderLineContent.charCodeAt(charIndex);
		let produced = 1;
		let charWidth = 1;
		switch (charCode) {
			case CharCode.Tab:
				produced = tabSize - (visibleColumn % tabSize);
				charWidth = produced;
				rendered += ' '.repeat(produced);
				break;
			case CharCode.Null:
			case CharCode.UTF8_BOM:
			case CharCode.LINE_SEPARATOR:
			case CharCode.PARAGRAPH_SEPARATOR:
			case CharCode.NEXT_LINE:
				rendered += '�';
				break;
			default:
				if (strings.isFullWidthCharacter(charCode)) {
					charWidth++;
				}
				rendered += charCode < 32 ? String.fromCharCode(9216 + charCode) : renderLineContent.charAt(charIndex);
		}
		charOffset += produced;
		visibleColumn += charWidth;
	}
	charOffsets[len] = charOffset;
	visibleColumns[len] = visibleColumn;

	const prepared = module.prepareWithSegments(rendered, font, options);

	// Offset (in `rendered`) where each segment starts. Pretext must preserve the text
	// verbatim in pre-wrap mode; if it does not, let the DOM computer handle the line.
	const segments = prepared.segments;
	const segmentStarts = new Array<number>(segments.length + 1);
	let acc = 0;
	for (let i = 0; i < segments.length; i++) {
		segmentStarts[i] = acc;
		acc += segments[i].length;
	}
	segmentStarts[segments.length] = acc;
	if (acc !== rendered.length) {
		return undefined;
	}

	const cursorToOffset = (cursor: { segmentIndex: number; graphemeIndex: number }): number => {
		const base = segmentStarts[cursor.segmentIndex] ?? rendered.length;
		if (cursor.graphemeIndex === 0 || cursor.segmentIndex >= segments.length) {
			return base;
		}
		const segment = segments[cursor.segmentIndex];
		let g = 0;
		for (const part of graphemeSegmenter.value.segment(segment)) {
			if (g === cursor.graphemeIndex) {
				return base + part.index;
			}
			g++;
		}
		return base + segment.length;
	};

	// Rendered offset -> model character index (break offsets are model offsets).
	const renderedToChar = new Int32Array(rendered.length + 1);
	for (let charIndex = 0; charIndex < len; charIndex++) {
		for (let o = charOffsets[charIndex]; o < charOffsets[charIndex + 1]; o++) {
			renderedToChar[o] = charIndex;
		}
	}
	renderedToChar[rendered.length] = len;

	const breakOffsets: number[] = [];
	let start: { segmentIndex: number; graphemeIndex: number } | undefined = undefined;
	let lineWidth = width;
	for (; ;) {
		const range = module.layoutNextLineRange(prepared, Math.max(1, lineWidth), start);
		if (!range) {
			break;
		}
		const endOffset = cursorToOffset(range.end);
		if (endOffset >= rendered.length) {
			break;
		}
		const charIndex = renderedToChar[endOffset];
		if (breakOffsets.length === 0 || charIndex > breakOffsets[breakOffsets.length - 1]) {
			breakOffsets.push(charIndex);
		}
		start = range.end;
		lineWidth = width - additionalIndentLength;
	}

	if (breakOffsets.length === 0) {
		return null;
	}
	breakOffsets.push(len);

	const breakOffsetsVisibleColumn = breakOffsets.map(offset => visibleColumns[offset]);
	if (firstNonWhitespaceIndex !== 0) {
		for (let j = 0; j < breakOffsets.length; j++) {
			breakOffsets[j] += firstNonWhitespaceIndex;
		}
	}
	return new ModelLineProjectionData(null, null, breakOffsets, breakOffsetsVisibleColumn, wrappedTextIndentLength + additionalIndentSize);
}
