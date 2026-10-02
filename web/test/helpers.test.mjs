import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const src = readFileSync(new URL('../app.js', import.meta.url), 'utf8');
const block = (n) => src.split(`// ${n}-helpers-start`)[1].split(`// ${n}-helpers-end`)[0];
const { authBannerText, notifItems, visibleBanner, bellItems } = new Function(`${block('auth')}; return { authBannerText, notifItems, visibleBanner, bellItems };`)();

test('authBannerText', () => {
  assert.equal(authBannerText({ moodle: 'ok', apple: 'ok' }), '');
  assert.equal(authBannerText(undefined), '');
  for (const m of ['expired', 'no-session', 'needs-browser']) assert.match(authBannerText({ moodle: m, apple: 'ok' }), /^Moodle: sessió caducada.*Renova$/);
  assert.equal(authBannerText({ moodle: 'ok', apple: 'auth', apple_auto: 'failed' }), "Recordatoris: sessió d'iCloud caducada");
  assert.equal(authBannerText({ moodle: 'ok', apple: 'cal', apple_error: 'x' }), 'Calendari: error en sincronitzar amb iCloud (x)');
  assert.match(authBannerText({ moodle: 'ok', apple: 'config' }), /icloud-glass setup/);
  assert.match(authBannerText({ moodle: 'ok', apple: 'err' }), /^Recordatoris: error/);
  assert.match(authBannerText({ moodle: 'expired', apple: 'auth', apple_auto: 'failed' }), /Moodle.*·.*iCloud/);
});

test('notifItems/visibleBanner', () => {
  assert.deepEqual(notifItems({ moodle: 'ok', apple: 'ok' }), []);
  assert.deepEqual(notifItems({ moodle: 'ok', apple: 'auth', apple_auto: 'retrying' }), [], 'auto re-import in progress');
  const it = notifItems({ moodle: 'expired', apple: 'auth', apple_auto: 'failed', apple_since: 7 });
  assert.deepEqual(it.map((i) => i.key), ['moodle:expired', 'apple:auth:7']);
  assert.deepEqual(it.map((i) => i.act), ['Renova', 'Torna a importar']);
  assert.equal(notifItems({ moodle: 'ok', apple: 'cal', apple_since: 3 })[0].act, '');
  assert.deepEqual(visibleBanner(it, new Set(['moodle:expired'])).map((i) => i.key), ['apple:auth:7']);
  assert.deepEqual(visibleBanner(it, new Set()), it);
});

const { hasText } = new Function(`${block('intro')}; return { hasText };`)();
test('hasText', () => {
  for (const x of [undefined, '', '  ', '<p> </p>', '<p>&nbsp;</p><br>']) assert.equal(hasText(x), false);
  assert.equal(hasText('<p>Hola</p>'), true);
  assert.equal(hasText('<p><img src=x></p>'), true);
});

const { enunciatMode } = new Function(`${block('intro')}; return { enunciatMode };`)();
test('enunciatMode', () => {
  assert.equal(enunciatMode('ready', true), 'shot');
  assert.equal(enunciatMode('ready', false), 'shot');
  assert.equal(enunciatMode('pending', true), 'wait');
  assert.equal(enunciatMode('none', true), 'text');
  assert.equal(enunciatMode('none', false), 'link');
  assert.equal(enunciatMode(undefined, false), 'link');
});

const { aminName } = new Function(`${block('amin')}; return { aminName };`)();
test('aminName', () => {
  assert.equal(aminName('Pràctica 1.docx'), 'Pràctica 1_amin.docx');
  assert.equal(aminName('a_amin.docx'), 'a_amin.docx');
  assert.equal(aminName('arxiu'), 'arxiu_amin');
  assert.equal(aminName('E3_RA1_Amin.docx'), 'E3_RA1_amin.docx');
  assert.equal(aminName('E3_Amin_v2.docx'), 'E3_Amin_v2.docx');
  assert.equal(aminName('Examination.pdf'), 'Examination_amin.pdf');
  assert.equal(aminName('x.tar.gz'), 'x.tar_amin.gz');
});

const { plainClick } = new Function(`${block('pop')}; return { plainClick };`)();
test('plainClick', () => {
  assert.equal(plainClick({ button: 0 }), true);
  for (const m of ['ctrlKey', 'metaKey', 'shiftKey', 'altKey']) assert.equal(plainClick({ button: 0, [m]: true }), false);
  assert.equal(plainClick({ button: 1 }), false);
  assert.equal(plainClick({ button: 0, defaultPrevented: true }), false);
});

const { stmtLinks, linkKind } = new Function(`${block('intro')}; return { stmtLinks, linkKind };`)();
test('stmtLinks/linkKind', () => {
  const html = `<a href="https://docs.google.com/document/d/1/edit?a=1&amp;b=2">x</a><a href='https://docs.google.com/document/d/1/edit?a=1&amp;b=2'>y</a><a href="mailto:a@b">m</a><a href="https://youtu.be/x">v</a>`;
  assert.deepEqual(stmtLinks(html), ['https://docs.google.com/document/d/1/edit?a=1&b=2', 'https://youtu.be/x']);
  assert.deepEqual(stmtLinks(''), []);
  assert.equal(linkKind('https://docs.google.com/document/d/1'), 'Google Docs');
  assert.equal(linkKind('https://docs.google.com/presentation/d/1'), 'Google Slides');
  assert.equal(linkKind('https://docs.google.com/spreadsheets/d/1'), 'Google Sheets');
  assert.equal(linkKind('https://drive.google.com/file/d/1'), 'Google Drive');
  assert.equal(linkKind('https://docs.google.com/forms/d/1'), 'Google Forms');
  assert.equal(linkKind('https://www.youtube.com/watch?v=1'), 'YouTube');
  assert.equal(linkKind('https://example.org/a'), 'example.org');
});

test('bellItems adds a non-dismissable overdue item', () => {
  const ok = { moodle: 'ok', apple: 'ok' };
  assert.deepEqual(bellItems(ok, 0), []);
  const [it] = bellItems(ok, 3);
  assert.equal(it.href, '#/tauler');
  assert.match(it.text, /3/);
  assert.equal(it.act, '');
  const all = bellItems({ moodle: 'expired', apple: 'ok' }, 1);
  assert.equal(all.length, 2);
  assert.equal(all[1].key, 'late');
});
