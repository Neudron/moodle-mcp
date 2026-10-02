import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const src = readFileSync(new URL('../app.js', import.meta.url), 'utf8');
const { hue, HUES } = new Function(`${src.split('// hue-helpers-start')[1].split('// hue-helpers-end')[0]}; return { hue, HUES };`)();
test('course index maps to fixed palette', () => {
  const keys = 'abcdefgh'.split('');
  const hs = keys.map((k) => hue(k, keys));
  assert.deepEqual(hs, HUES);
  assert.equal(new Set(hs).size, 8);
  assert.equal(hue('c', keys), hue('c', keys));
});
test('unknown key falls back to palette', () => {
  const x = hue('zz', ['a']);
  assert.ok(HUES.includes(x));
  assert.equal(x, hue('zz'));
});
