import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const HUES = new Function(`${readFileSync(new URL('../app.js', import.meta.url), 'utf8').match(/const HUES = .*;/)[0]}; return HUES;`)();
const css = readFileSync(new URL('../app.css', import.meta.url), 'utf8');
const tok = (block, n) => (block.match(new RegExp(`--${n}:(#[0-9A-Fa-f]{6})`)) || [])[1];
const dark = css.split(':root{')[1].split('}')[0];
const light = css.split(':root[data-theme=light]{')[1].split('}')[0];
const lum = ([r, g, b]) => [r, g, b].map((v) => (v /= 255) <= .03928 ? v / 12.92 : ((v + .055) / 1.055) ** 2.4).reduce((a, v, i) => a + v * [.2126, .7152, .0722][i], 0);
const rgb = (x) => [1, 3, 5].map((i) => parseInt(x.slice(i, i + 2), 16));
const ratio = (a, b) => { const [x, y] = [lum(a), lum(b)].sort((p, q) => q - p); return (x + .05) / (y + .05); };
function hsl(h, s, l) { s /= 100; l /= 100; const k = (n) => (n + h / 30) % 12, a = s * Math.min(l, 1 - l); return [0, 8, 4].map((n) => 255 * (l - a * Math.max(-1, Math.min(k(n) - 3, 9 - k(n), 1)))); }
const pair = (b, n) => b.match(new RegExp(`--${n}:(\\d+)% (\\d+)%`)).slice(1).map(Number);
for (const [name, b] of [['dark', dark], ['light', light]]) {
  const [ct, cs] = [pair(b, 'ct'), pair(b, 'cs')];
  const get = (n) => tok(b, n) || tok(dark, n);
  const bgs = ['page', 'raised', 'sunk', 'hover'].map((n) => rgb(get(n)));
  test(`${name}: text tokens 4.5:1`, () => {
    for (const t of ['text', 'dim', 'accent', 'late', 'soon', 'stale', 'ok']) for (const bg of bgs) assert.ok(ratio(rgb(get(t)), bg) >= 4.5, `${name} ${t} ${ratio(rgb(get(t)), bg).toFixed(2)}`);
  });
  test(`${name}: ctl 3:1`, () => {
    for (const t of ['ctl']) for (const bg of bgs) assert.ok(ratio(rgb(get(t)), bg) >= 3, `${name} ${t} ${ratio(rgb(get(t)), bg).toFixed(2)}`);
  });
  test(`${name}: course hue text 4.5:1, dot 3:1`, () => {
    for (const h of HUES) for (const bg of bgs) {
      assert.ok(ratio(hsl(h, ...ct), bg) >= 4.5, `${name} cn h=${h} ${ratio(hsl(h, ...ct), bg).toFixed(2)}`);
      assert.ok(ratio(hsl(h, ...cs), bg) >= 3, `${name} dot h=${h} ${ratio(hsl(h, ...cs), bg).toFixed(2)}`);
    }
  });
}
