import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const src = readFileSync(new URL('../app.js', import.meta.url), 'utf8');
const block = (n) => src.split(`// ${n}-helpers-start`)[1].split(`// ${n}-helpers-end`)[0];
const { agenda, gridMove } = new Function(`const dayKey = (ts) => new Date(ts * 1000).toLocaleDateString('en-CA', { timeZone: 'Europe/Madrid' }); ${block('agenda')}; return { agenda, gridMove };`)();

const D = 86400, now = Date.UTC(2026, 9, 1, 10) / 1000;
const t = (id, off) => ({ task: { id, due: off == null ? null : now + off * D } });
test('agenda: late first, days sorted, window, no undated', () => {
  const g = agenda([t('c', 3), t('a', -2), t('b', 0), t('x', 20), t('n', null), t('d', 3)], now, 14);
  assert.deepEqual(g.map((x) => x.day), ['late', '2026-10-01', '2026-10-04']);
  assert.deepEqual(g[2].items.map((x) => x.task.id), ['c', 'd']);
  assert.deepEqual(agenda([], now), []);
});
test('gridMove: arrows, home/end, bounds', () => {
  assert.equal(gridMove(8, 'ArrowRight'), 9);
  assert.equal(gridMove(8, 'ArrowDown'), 15);
  assert.equal(gridMove(8, 'ArrowUp'), 1);
  assert.equal(gridMove(0, 'ArrowLeft'), 0);
  assert.equal(gridMove(40, 'ArrowDown'), 40);
  assert.equal(gridMove(9, 'Home'), 7);
  assert.equal(gridMove(9, 'End'), 13);
  assert.equal(gridMove(9, 'q'), -1);
});
