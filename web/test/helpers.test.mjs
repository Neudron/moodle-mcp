import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const src = readFileSync(new URL('../app.js', import.meta.url), 'utf8');
const block = (n) => src.split(`// ${n}-helpers-start`)[1].split(`// ${n}-helpers-end`)[0];
const { authBannerText } = new Function(`${block('auth')}; return { authBannerText };`)();

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
