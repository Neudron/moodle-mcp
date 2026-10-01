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
