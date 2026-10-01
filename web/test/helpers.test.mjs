import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const src = readFileSync(new URL('../app.js', import.meta.url), 'utf8');
const block = (n) => src.split(`// ${n}-helpers-start`)[1].split(`// ${n}-helpers-end`)[0];
const { authBannerText, notifItems, visibleBanner } = new Function(`${block('auth')}; return { authBannerText, notifItems, visibleBanner };`)();

test('authBannerText', () => {
  assert.equal(authBannerText({ moodle: 'ok', apple: 'ok' }), '');
  assert.equal(authBannerText(undefined), '');
  for (const m of ['expired', 'no-session', 'needs-browser']) assert.match(authBannerText({ moodle: m, apple: 'ok' }), /^Moodle: sessió caducada.*Renova$/);
  assert.equal(authBannerText({ moodle: 'ok', apple: 'auth' }), "Recordatoris: sessió d'iCloud caducada");
  assert.equal(authBannerText({ moodle: 'ok', apple: 'cal', apple_error: 'x' }), 'Calendari: error en sincronitzar amb iCloud (x)');
  assert.match(authBannerText({ moodle: 'ok', apple: 'config' }), /icloud-glass setup/);
  assert.match(authBannerText({ moodle: 'ok', apple: 'err' }), /^Recordatoris: error/);
  assert.match(authBannerText({ moodle: 'expired', apple: 'auth' }), /Moodle.*·.*iCloud/);
});

test('notifItems/visibleBanner', () => {
  assert.deepEqual(notifItems({ moodle: 'ok', apple: 'ok' }), []);
  const it = notifItems({ moodle: 'expired', apple: 'auth', apple_since: 7 });
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
