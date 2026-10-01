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
  for (const a of ['auth', 'err']) assert.equal(authBannerText({ moodle: 'ok', apple: a }), "Recordatoris: sessió d'iCloud caducada");
  assert.match(authBannerText({ moodle: 'expired', apple: 'auth' }), /Moodle.*·.*iCloud/);
});
