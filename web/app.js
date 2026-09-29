'use strict';
// moodle-web frontend. No build step. Remote HTML only via sandboxed iframes.
const $ = (s, el = document) => el.querySelector(s);

function h(tag, attrs = {}, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
    else if (k === 'style' && typeof v === 'object') for (const [p, val] of Object.entries(v)) p.startsWith('--') ? el.style.setProperty(p, val) : (el.style[p] = val);
    else if (v === true) el.setAttribute(k, '');
    else if (v !== false && v != null) el.setAttribute(k, v);
  }
  for (const k of kids.flat(Infinity)) if (k != null && k !== false) el.append(k instanceof Node ? k : String(k));
  return el;
}

async function api(path, opts = {}) {
  const o = { ...opts, headers: { ...(opts.headers || {}) } };
  if (o.body !== undefined && typeof o.body !== 'string') { o.body = JSON.stringify(o.body); o.headers['content-type'] = 'application/json'; }
  if (o.method && o.method !== 'GET') o.headers['x-neu'] = '1';
  const r = await fetch(path, o);
  if (!r.ok) throw new Error((await r.text()) || r.statusText);
  return (r.headers.get('content-type') || '').includes('json') ? r.json() : r.text();
}

const TZ = 'Europe/Madrid';
const fmtLong = new Intl.DateTimeFormat('ca-ES', { timeZone: TZ, weekday: 'short', day: 'numeric', month: 'short', hour: '2-digit', minute: '2-digit' });
const when = (ts) => (ts ? fmtLong.format(new Date(ts * 1000)) : 'Sense data');
function left(ts) {
  if (!ts) return { txt: '—', cls: '' };
  const hrs = (ts * 1000 - Date.now()) / 36e5;
  if (hrs < 0) return { txt: 'endarrerida', cls: 'late' };
  if (hrs < 48) return { txt: `${Math.max(1, Math.round(hrs))} h`, cls: 'soon' };
  return { txt: `${Math.round(hrs / 24)} dies`, cls: '' };
}
function hue(s) { let x = 0; for (const c of s || '') x = (x * 31 + c.codePointAt(0)) % 360; return x; }
const isDone = (tv) => tv.local.status === 'lliurat' || tv.task.moodle_status === 'submitted';
const KIND = { assign: 'Tasca', quiz: 'Qüestionari', post: 'Avís', ai: 'IA' };
const MSTATUS = { new: 'Sense enviar', draft: 'Esborrany', submitted: 'Enviada' };
const enc = encodeURIComponent;

function sandboxed(html) {
  const css = '<style>body{font:16px/1.5 system-ui,sans-serif;margin:16px;color:#1b1722}img{max-width:100%}</style>';
  return h('iframe', { class: 'body', sandbox: '', referrerpolicy: 'no-referrer', title: 'Contingut de Moodle', srcdoc: css + (html || '<p>(sense descripció)</p>') });
}

function toast(msg) {
  const t = $('#toast'); t.textContent = msg; t.classList.add('on');
  clearTimeout(toast.t); toast.t = setTimeout(() => t.classList.remove('on'), 3000);
}

function confirmDialog(title, body, okLabel = "D'acord") {
  const d = $('#dlg');
  $('#dlg-t').textContent = title;
  $('#dlg-b').replaceChildren(body);
  $('#dlg-ok').textContent = okLabel;
  d.returnValue = '';
  d.showModal();
  return new Promise((res) => d.addEventListener('close', () => res(d.returnValue === 'ok'), { once: true }));
}

const views = {};
const taskExtras = [];
const fileExtras = [];

function card(tv) {
  const t = tv.task, l = left(t.due);
  const badges = [
    h('span', { class: 'badge' + (t.moodle_status === 'submitted' ? ' ok' : '') }, MSTATUS[t.moodle_status] || t.moodle_status),
    tv.local.status && h('span', { class: 'badge ' + (tv.local.status === 'revisat OK' ? 'ok' : 'warn') }, tv.local.status),
    t.source === 'ai' && h('span', { class: 'badge ai' }, 'extret per IA'),
  ];
  return h('a', { class: 'card', href: `#/tasca/${enc(t.id)}` },
    h('span', { class: 'dot', style: { '--h': hue(t.course) } }),
    h('span', {}, h('span', { class: 't' }, t.title), badges, h('br'),
      h('span', { class: 'm' }, t.course, ' · ', KIND[t.kind] || t.kind, ' · ', when(t.due))),
    h('span', { class: `left ${isDone(tv) ? '' : l.cls}` }, isDone(tv) ? '✓' : l.txt));
}

views.tauler = async () => {
  const all = (await api('/api/tasks')).filter((tv) => tv.task.kind !== 'post' && tv.task.ai_confirmed !== false);
  const now = Date.now() / 1000, week = now + 7 * 86400;
  const groups = [
    ['Endarrerides', (tv) => !isDone(tv) && tv.task.due && tv.task.due < now],
    ['Aquesta setmana', (tv) => !isDone(tv) && tv.task.due >= now && tv.task.due <= week],
    ['Més endavant', (tv) => !isDone(tv) && (!tv.task.due || tv.task.due > week)],
    ['Lliurades', isDone],
  ];
  const pending = all.filter((tv) => !isDone(tv)).length;
  return h('section', {}, h('h1', {}, `Tens ${pending} ${pending === 1 ? 'tasca pendent' : 'tasques pendents'}`),
    groups.map(([name, f]) => {
      const items = all.filter(f);
      if (name === 'Lliurades') items.reverse();
      return items.length ? [h('h2', {}, `${name} · ${items.length}`), h('div', { class: 'cards' }, items.map(card))] : null;
    }));
};

views.tasca = async (id) => {
  const tv = await api(`/api/tasks/${enc(id)}`);
  const t = tv.task, l = tv.local;
  const status = h('select', { 'aria-label': 'Estat local', onchange: async (e) => {
    await api(`/api/tasks/${enc(id)}/status`, { method: 'POST', body: { status: e.target.value } }); toast('Estat desat'); } },
    ['', 'per revisar', 'revisat OK', 'lliurat'].map((s) => h('option', { value: s, selected: s === l.status }, s || '— sense estat —')));
  const actions = h('div', { class: 'actions' },
    h('a', { href: t.url, target: '_blank', rel: 'noopener' }, h('button', { type: 'button' }, 'Obre a Moodle')),
    status,
    t.source === 'ai' && t.ai_confirmed == null && [
      h('button', { type: 'button', onclick: async () => { await api(`/api/tasks/${enc(id)}/ai`, { method: 'POST', body: { keep: true } }); route(); } }, 'Mantén'),
      h('button', { type: 'button', onclick: async () => { await api(`/api/tasks/${enc(id)}/ai`, { method: 'POST', body: { keep: false } }); location.hash = '#/tauler'; } }, 'Descarta')],
    taskExtras.map((f) => f(tv)));
  const atts = t.attachments.map((a) => h('li', {}, h('a', { href: `/api/moodle-file?url=${enc(a.fileurl)}`, target: '_blank' }, a.filename)));
  const files = l.files.map((f) => h('li', {}, h('a', { href: `#/fitxers/${enc(f)}` }, f)));
  return h('article', {},
    h('p', {}, h('a', { href: '#/tauler' }, '← Tauler')),
    h('h1', {}, t.title),
    h('div', { class: 'meta' },
      h('span', {}, h('span', { class: 'dot', style: { '--h': hue(t.course), display: 'inline-block' } }), ' ', t.course),
      h('span', {}, 'Venciment: ', when(t.due)),
      t.cutoff ? h('span', {}, 'Tancament: ', when(t.cutoff)) : null,
      h('span', {}, 'Moodle: ', MSTATUS[t.moodle_status] || t.moodle_status),
      t.grade && h('span', {}, 'Nota: ', t.grade)),
    actions,
    sandboxed(t.body_html),
    atts.length ? [h('h2', {}, 'Adjunts del professor'), h('ul', { class: 'list' }, atts)] : null,
    t.feedback_html ? [h('h2', {}, 'Retroacció'), sandboxed(t.feedback_html)] : null,
    h('h2', {}, 'Els meus fitxers'),
    files.length ? h('ul', { class: 'list' }, files) : h('p', { class: 'dim' }, `Cap fitxer vinculat. Carpeta suggerida: ${tv.submit_dir}`),
    linkFiles(id, l.files));
};

function linkFiles(id, current) {
  const input = h('input', { list: 'submit-files', placeholder: 'submit/…/fitxer_amin.docx', 'aria-label': 'Fitxer a vincular', style: { minWidth: '320px' } });
  const dl = h('datalist', { id: 'submit-files' });
  api('/api/submit-files').then((fs) => dl.replaceChildren(...fs.map((f) => h('option', { value: f }))));
  return h('div', { class: 'actions' }, input, dl, h('button', { type: 'button', onclick: async () => {
    if (!input.value) return;
    try { await api(`/api/tasks/${enc(id)}/files`, { method: 'POST', body: { files: [...current, input.value] } }); route(); }
    catch (e) { toast(e.message); }
  } }, 'Vincula fitxer'));
}

async function route() {
  const [name, ...rest] = (location.hash.slice(2) || 'tauler').split('/');
  const main = $('#main');
  document.querySelectorAll('nav a').forEach((a) => (a.getAttribute('href') === `#/${name}` ? a.setAttribute('aria-current', 'page') : a.removeAttribute('aria-current')));
  main.replaceChildren(h('p', { class: 'dim' }, 'Carregant…'));
  try { main.replaceChildren(await (views[name] || views.tauler)(...rest.map(decodeURIComponent))); }
  catch (e) { main.replaceChildren(h('p', { class: 'err' }, e.message)); }
  main.focus({ preventScroll: true });
}

async function refreshStatus() {
  try {
    const s = await api('/api/status');
    $('#lastsync').textContent = s.syncing ? 'Sincronitzant…' : s.last_sync ? `Última: ${when(s.last_sync)}` : 'Mai sincronitzat';
    const b = $('#banner'); b.hidden = !s.error; b.textContent = s.error;
  } catch (e) { /* server restarting */ }
}

async function loadCourses() {
  const cs = await api('/api/courses');
  $('#courses').replaceChildren(...cs.map((c) => h('a', { href: `#/curs/${enc(c.shortname)}` }, h('span', { class: 'dot', style: { '--h': hue(c.shortname) } }), c.title)));
}

$('#sync').addEventListener('click', async (e) => {
  const b = e.currentTarget; b.disabled = true;
  try { const r = await api('/api/sync', { method: 'POST' }); toast(`Sincronitzat (${r.events} canvis)`); route(); }
  catch (err) { toast(err.message); }
  b.disabled = false; refreshStatus();
});
$('#theme').addEventListener('click', () => {
  const cur = document.documentElement.dataset.theme || (matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark');
  const next = cur === 'light' ? 'dark' : 'light';
  document.documentElement.dataset.theme = next;
  try { localStorage.setItem('theme', next); } catch (e) { /* private mode */ }
});
try { const t = localStorage.getItem('theme'); if (t) document.documentElement.dataset.theme = t; } catch (e) { /* ignore */ }
addEventListener('hashchange', route);
addEventListener('DOMContentLoaded', () => { route(); refreshStatus(); loadCourses(); setInterval(refreshStatus, 60000); });
