import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const src = readFileSync(new URL('../app.js', import.meta.url), 'utf8');
const block = (n) => src.split(`// ${n}-helpers-start`)[1].split(`// ${n}-helpers-end`)[0];
const { filterPalette } = new Function(`${block('palette')}; return { filterPalette };`)();

const it = (...l) => l.map((label) => ({ label }));
test('filterPalette', () => {
  const items = it('Notes', 'Fitxer de notes', 'Calendari', 'Cançó NOTA');
  assert.deepEqual(filterPalette(items, 'not').map((x) => x.label), ['Notes', 'Fitxer de notes', 'Cançó NOTA']);
  assert.deepEqual(filterPalette(items, 'NOTA').map((x) => x.label), ['Cançó NOTA']);
  assert.deepEqual(filterPalette(items, '  ').map((x) => x.label), items.map((x) => x.label));
  assert.deepEqual(filterPalette(items, 'zzz'), []);
  assert.equal(filterPalette(Array.from({ length: 100 }, (_, i) => ({ label: `a${i}` })), 'a').length, 50);
});
