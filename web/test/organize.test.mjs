import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const src = readFileSync(new URL('../app.js', import.meta.url), 'utf8');
const { groupOps } = new Function(`${src.split('// ops-helpers-start')[1].split('// ops-helpers-end')[0]}; return { groupOps };`)();
test('groups by kind and parent dir, keeps only changing part', () => {
  const g = groupOps([
    { kind: 'move', from: 'a/b/x.pdf', to: 'a/b/y.pdf' },
    { kind: 'move', from: 'a/b/z.pdf', to: 'a/c/z.pdf' },
    { kind: 'move', from: 'a/q.pdf', to: '.moodle/trash/123/a/q.pdf' },
    { kind: 'replace', from: 'a/b/w.pdf', to: 'a/b/w2.pdf' },
  ]);
  assert.equal(g.length, 3);
  assert.deepEqual(g[0], { kind: 'move', dir: 'a/b', rows: [{ from: 'x.pdf', to: 'y.pdf' }, { from: 'z.pdf', to: 'a/c/z.pdf' }] });
  assert.equal(g[1].dir, 'a');
  assert.equal(g[1].rows[0].to, 'paperera/a/q.pdf');
  assert.equal(g[2].kind, 'replace');
});
test('root-level file has empty dir', () => {
  assert.equal(groupOps([{ kind: 'move', from: 'x', to: 'y' }])[0].dir, '');
});
