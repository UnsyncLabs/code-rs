# Pretext line wrapping (experiment → adopted)

`editor.wrappingStrategy: "advanced"` used to compute wrap points by rendering every
line into a hidden DOM node and binary-searching `getClientRects()`
(`domLineBreaksComputer.ts`). code-rs now uses
[Pretext](https://github.com/chenglou/pretext) (`@chenglou/pretext`, MIT) instead:
segments are measured once with canvas (cached) and lines are broken with arithmetic,
no DOM layout or reflow.

- Implementation: `src/vs/editor/browser/view/pretextLineBreaksComputer.ts`
- Wired in: `codeEditorWidget.ts` (replaces `DOMLineBreaksComputerFactory`)
- Fallback: lines with injected text, two-cell fullwidth rendering, or before Pretext has
  loaded go through the original DOM computer. Monospace "simple" wrapping is unchanged.

## Benchmark (WebView2, code-rs dev build, 2026-11-08)

`scripts/code-rs/pretext-wrap-benchmark.js`, run inside the app via CDP. Input:
54,042 lines / 2.3 MB of JSON (pretty-printed, long strings, minified chunks, 2,000 lines
with emoji, Chinese, Arabic and mixed scripts), Segoe UI 14px, median of 3 runs.

| Scenario | DOM (ms) | Pretext (ms) | Speed-up | Identical breaks |
|---|---|---|---|---|
| Wrap at 80 columns | 1021 | 149 | 6.9× | 100% |
| Wrap at 120 columns | 967 | 141 | 6.8× | 100% |
| Re-wrap at 4 widths (resize) | 4023 | 687 | 5.9× | — |

With Consolas (monospace): 986 → 118 ms at 80 columns (8.4×), 100% identical.
Lines delegated to the DOM fallback during the benchmark: 0.

## Packaging note

Pretext is loaded at runtime from `node_modules/@chenglou/pretext/dist/layout.js`; the
production bundle must ship that folder (or inline it).
