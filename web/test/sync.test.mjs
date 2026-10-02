import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const src = readFileSync(new URL('../app.js', import.meta.url), 'utf8');
const block = (n) => src.split(`// ${n}-helpers-start`)[1].split(`// ${n}-helpers-end`)[0];
const { syncProgress, syncSummary } = new Function(`${block('sync')}; return { syncProgress, syncSummary };`)();

test('syncProgress: phase, i/n and course', () => {
  assert.equal(syncProgress({ phase: 'files', i: 3, n: 8, course: 'bbdd' }), 'Sincronitzant: fitxers 3/8 · bbdd');
  assert.equal(syncProgress({ phase: 'tasks', i: 1, n: 2, course: 'x' }), 'Sincronitzant: tasques 1/2 · x');
  assert.equal(syncProgress(null), 'Sincronitzant…');
});
test('syncSummary: counts, errors', () => {
  assert.equal(syncSummary({ events: 2, files_new: 3, files_removed: 1, errors: [] }), 'Sincronitzat: 2 canvis, 3 fitxers nous, 1 eliminats (a la paperera)');
  assert.equal(syncSummary({ events: 0, files_new: 0, files_removed: 0, errors: ['a', 'b'] }), 'Sincronitzat: 0 canvis, 0 fitxers nous, 0 eliminats (a la paperera). 2 errors');
  assert.equal(syncSummary(null), 'Sincronitzat');
});
