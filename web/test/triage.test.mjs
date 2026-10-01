import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const src = readFileSync(new URL('../app.js', import.meta.url), 'utf8');
const block = (n) => src.split(`// ${n}-helpers-start`)[1].split(`// ${n}-helpers-end`)[0];
const { rankProposals, groupByDue, shouldSoftRoute } = new Function(`${block('triage')}; return { rankProposals, groupByDue, shouldSoftRoute };`)();

const p = (id, s) => ({ task: { id, ai_score: s } });
test('rankProposals', () => {
  const list = [p('a', 0.6), p('b', 0), p('c', 0.9), p('d', 0.6), p('e', 0.4), p('f', null)];
  const { top, rest } = rankProposals(list);
  assert.deepEqual(top.map((x) => x.task.id), ['c', 'a', 'd']); // desc, stable ties
  assert.deepEqual(rest.map((x) => x.task.id), ['e', 'b', 'f']);
  const many = Array.from({ length: 15 }, (_, i) => p(`x${i}`, 0.8));
  const r = rankProposals(many);
  assert.equal(r.top.length, 10);
  assert.equal(r.rest.length, 5);
  assert.deepEqual(rankProposals([]), { top: [], rest: [] });
});

const NOW = Date.parse('2026-10-01T10:00:00+02:00') / 1000;
const t = (id, due) => ({ task: { id, due } });
test('groupByDue', () => {
  const g = groupByDue([
    t('far', NOW + 30 * 86400), t('nodue', 0), t('tom', NOW + 86400), t('today-late', NOW - 3600), t('today-eve', Date.parse('2026-10-01T23:59:00+02:00') / 1000),
    t('yest', NOW - 86400), t('earlier', NOW - 5 * 86400), t('d6', NOW + 6 * 86400), t('d7', NOW + 7 * 86400),
  ], NOW);
  assert.deepEqual(g.late.map((x) => x.task.id), ['earlier', 'yest']);
  assert.deepEqual(g.today.map((x) => x.task.id), ['today-late', 'today-eve']);
  assert.deepEqual(g.week.map((x) => x.task.id), ['tom', 'd6']);
  assert.deepEqual(g.later.map((x) => x.task.id), ['d7', 'far', 'nodue']);
});
test('groupByDue day boundary (Madrid)', () => {
  const just = Date.parse('2026-10-01T00:00:30+02:00') / 1000, before = Date.parse('2026-09-30T23:59:30+02:00') / 1000;
  const g = groupByDue([t('a', just), t('b', before)], NOW);
  assert.equal(g.today[0].task.id, 'a');
  assert.equal(g.late[0].task.id, 'b');
});

test('shouldSoftRoute', () => {
  assert.equal(shouldSoftRoute(1, 2, 'BODY', 'tauler'), true);
  assert.equal(shouldSoftRoute(undefined, 2, 'BODY', 'tauler'), false);
  assert.equal(shouldSoftRoute(2, 2, 'BODY', 'tauler'), false);
  assert.equal(shouldSoftRoute(1, 0, 'BODY', 'tauler'), false);
  for (const tag of ['INPUT', 'TEXTAREA', 'SELECT']) assert.equal(shouldSoftRoute(1, 2, tag, 'tauler'), false);
  for (const v of ['edita', 'ide']) assert.equal(shouldSoftRoute(1, 2, 'BODY', v), false);
});
