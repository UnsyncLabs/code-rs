(async () => {
  const base = '/static/out/vs/editor/';
  const { FontMeasurements } = await import(base + 'browser/config/fontMeasurements.js');
  const { createBareFontInfoFromRawSettings } = await import(base + 'common/config/fontInfoFromSettings.js');
  const { DOMLineBreaksComputerFactory } = await import(base + 'browser/view/domLineBreaksComputer.js');
  const P = await import(base + 'browser/view/pretextLineBreaksComputer.js');
  await P.loadPretext();
  const fontInfo = FontMeasurements.readFontInfo(window, createBareFontInfoFromRawSettings({ fontFamily: '"Segoe UI", sans-serif', fontSize: 14, lineHeight: 19 }, 1));

  // Large pretty-printed JSON with long string values, plus a minified chunk.
  const rnd = (n) => { let s = ''; const w = ['lorem','ipsum','dolor','sit','amet','consectetur','adipiscing','elit','sed','do','eiusmod','tempor','incididunt','ut','labore','et','dolore','magna','aliqua','https://example.com/api/v1/resource?id=']; for (let i = 0; i < n; i++) s += w[(i * 7 + n) % w.length] + (i % 5 ? ' ' : '-'); return s; };
  const items = []; for (let i = 0; i < 4000; i++) items.push({ id: i, name: 'item_' + i, description: rnd(10 + (i % 60)), tags: ['a' + i, 'b' + i], nested: { url: 'https://example.com/' + rnd(3).replace(/ /g, '/') + i, ok: i % 2 === 0 } });
  const lines = JSON.stringify(items, null, '\t').split('\n');
  const minified = JSON.stringify(items.slice(0, 300));
  for (let i = 0; i < 2000; i++) lines.push('	"msg": "' + ['Hola 👋 mundo 🚀 con emoji y más texto ', '春天到了，我们一起去公园散步吧，天气很好 ', 'بدأت الرحلة في الصباح الباكر مع الأصدقاء ', 'mixed English 中文 العربية 🎉 text '][i % 4].repeat(1 + i % 6) + '",');
  for (let i = 0; i < 40; i++) lines.push(minified.substring(i * 2000, i * 2000 + 2000));
  const ctx = { getLineContent: n => lines[n - 1], getLineInjectedText: () => null };

  const run = (factory, col) => {
    const c = factory.createLineBreaksComputer(ctx, fontInfo, 4, col, 1 /* Same */, 'normal', false, false);
    for (let n = 1; n <= lines.length; n++) c.addRequest(n, null);
    return c.finalize();
  };
  const dom = DOMLineBreaksComputerFactory.create(window);
  let fallbackCount = 0;
  const pre = new P.PretextLineBreaksComputerFactory({ createLineBreaksComputer: (...a) => { const d = dom.createLineBreaksComputer(...a); return { addRequest: (n, p) => { fallbackCount++; d.addRequest(n, p); }, finalize: () => d.finalize() }; } });
  const out = { lines: lines.length, chars: lines.reduce((a, l) => a + l.length, 0) };
  for (const col of [80, 120]) {
    // warm-up
    run(dom, col); run(pre, col);
    const t = (f) => { const t0 = performance.now(); const r = f(); return [performance.now() - t0, r]; };
    const domTimes = [], preTimes = []; let rd, rp;
    for (let k = 0; k < 3; k++) { const [a, r1] = t(() => run(dom, col)); domTimes.push(a); rd = r1; const [b, r2] = t(() => run(pre, col)); preTimes.push(b); rp = r2; }
    let same = 0, wrapped = 0, diffs = [];
    for (let i = 0; i < lines.length; i++) {
      const a = rd[i]?.breakOffsets?.join(',') ?? '', b = rp[i]?.breakOffsets?.join(',') ?? '';
      if (a || b) wrapped++;
      if (a === b) same++; else if (diffs.length < 3) diffs.push({ line: i + 1, dom: a.slice(0, 60), pretext: b.slice(0, 60) });
    }
    const med = (x) => x.sort((p, q) => p - q)[1].toFixed(1);
    out['col' + col] = { domMs: med(domTimes), pretextMs: med(preTimes), wrappedLines: wrapped, identical: (100 * same / lines.length).toFixed(2) + '%', diffs };
  }
  // Resize scenario: re-wrap at many widths (pretext cache warm)
  const t0 = performance.now(); for (const c of [70, 90, 100, 110]) run(dom, c); const tDom = performance.now() - t0;
  const t1 = performance.now(); for (const c of [70, 90, 100, 110]) run(pre, c); const tPre = performance.now() - t1;
  out.resize4widths = { domMs: tDom.toFixed(1), pretextMs: tPre.toFixed(1) };
  out.fallbackLinesTotal = fallbackCount; return JSON.stringify(out);
})()
