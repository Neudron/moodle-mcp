import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const src = readFileSync(new URL('../app.js', import.meta.url), 'utf8');
const block = (n) => src.split(`// ${n}-helpers-start`)[1].split(`// ${n}-helpers-end`)[0];
const { crumbsOf, pickMove } = new Function(`${block('picker')}; return { crumbsOf, pickMove };`)();

test('crumbsOf', () => {
  assert.deepEqual(crumbsOf(''), [{ name: '~', rel: '' }]);
  assert.deepEqual(crumbsOf('smx/submit'), [{ name: '~', rel: '' }, { name: 'smx', rel: 'smx' }, { name: 'submit', rel: 'smx/submit' }]);
});
test('pickMove', () => {
  assert.equal(pickMove(0, 'ArrowDown', 3), 1); assert.equal(pickMove(2, 'ArrowDown', 3), 2);
  assert.equal(pickMove(1, 'Home', 3), 0); assert.equal(pickMove(0, 'End', 3), 2);
  assert.equal(pickMove(0, 'PageDown', 30), 10); assert.equal(pickMove(0, 'x', 3), -1);
  assert.equal(pickMove(5, 'PageUp', 30), 0); assert.equal(pickMove(2, 'ArrowUp', 3), 1);
});

const { backFor, stripTitle, startDirs } = new Function(`${block('back')}; ${block('picker')}; return { backFor, stripTitle, startDirs };`)();
test('backFor', () => {
  assert.deepEqual(backFor('curs', '#/curs/x', 'DAW'), { hash: '#/curs/x', label: 'DAW' });
  assert.deepEqual(backFor('tauler', '#/tauler', ''), { hash: '#/tauler', label: 'Tauler' });
  assert.equal(backFor('tauler', '#/tauler', 'Tens 5 tasques pendents').label, 'Tauler');
  assert.equal(backFor('calendari', '#/calendari/2026-10', 'Octubre de 2026').label, 'Calendari');
  assert.equal(backFor('tasca', '#/tasca/1', 'T'), null);
});
test('stripTitle', () => {
  assert.equal(stripTitle('<p><b>Tasca 1. (a)</b></p><p>Text</p>', 'Tasca 1. (a)'), '<p>Text</p>');
  assert.equal(stripTitle('<p>Altre</p>', 'Tasca'), '<p>Altre</p>');
});
test('startDirs', () => {
  assert.deepEqual(startDirs('smx/submit/a/b', 'q'), ['smx/submit/a/b', 'smx/submit/a', 'smx/submit', 'smx', 'q']);
  assert.deepEqual(startDirs(undefined, null), ['smx/submit']);
});
