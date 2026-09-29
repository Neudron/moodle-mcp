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
  // Overdue by more than 60 days = last school year's leftovers: collapsed, not counted as pending.
  const stale = (tv) => !isDone(tv) && tv.task.due && tv.task.due < now - 60 * 86400;
  const groups = [
    ['Endarrerides', (tv) => !isDone(tv) && !stale(tv) && tv.task.due && tv.task.due < now],
    ['Aquesta setmana', (tv) => !isDone(tv) && tv.task.due >= now && tv.task.due <= week],
    ['Més endavant', (tv) => !isDone(tv) && (!tv.task.due || tv.task.due > week)],
    ['Lliurades', isDone],
  ];
  const pending = all.filter((tv) => !isDone(tv) && !stale(tv)).length;
  const old = all.filter(stale);
  return h('section', {}, h('h1', {}, `Tens ${pending} ${pending === 1 ? 'tasca pendent' : 'tasques pendents'}`),
    groups.map(([name, f]) => {
      const items = all.filter(f);
      if (name === 'Lliurades') items.reverse();
      return items.length ? [h('h2', {}, `${name} · ${items.length}`), h('div', { class: 'cards' }, items.map(card))] : null;
    }),
    old.length ? h('details', {}, h('summary', {}, `Antigues sense enviar · ${old.length}`), h('div', { class: 'cards' }, old.map(card))) : null);
};

views.tasca = async (id) => {
  const tv = await api(`/api/tasks/${enc(id)}`);
  const t = tv.task, l = tv.local;
  // Save only on an explicit click: arrow keys on a closed <select> fire `change` per step,
  // and 'lliurat' moves the submit folder, so it also needs a confirmation.
  const select = h('select', { 'aria-label': 'Estat local' },
    ['', 'per revisar', 'revisat OK', 'lliurat'].map((s) => h('option', { value: s, selected: s === l.status }, s || '— sense estat —')));
  const save = h('button', { type: 'button', id: 'estat-desa', onclick: async () => {
    const next = select.value;
    if (next === l.status) return;
    if (next === 'lliurat' && !(await confirmDialog('Marcar com a lliurada?',
      h('p', {}, `La carpeta de "${t.title}" es mourà a submit/_lliurat/.`), 'Marca lliurada'))) { select.value = l.status; save.focus(); return; }
    try { await api(`/api/tasks/${enc(id)}/status`, { method: 'POST', body: { status: next } }); toast('Estat desat'); await route(); $('#estat-desa')?.focus(); }
    catch (err) { toast(err.message); select.value = l.status; }
  } }, 'Desa estat');
  const status = h('span', { class: 'actions', style: { margin: '0' } }, select, save);
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
$('.skip').addEventListener('click', (e) => { e.preventDefault(); $('#main').focus(); });
addEventListener('hashchange', route);
addEventListener('DOMContentLoaded', () => { route(); refreshStatus(); loadCourses(); setInterval(refreshStatus, 60000); });

views.curs = async (short) => {
  const c = await api(`/api/courses/${enc(short)}`);
  const tasks = (await api('/api/tasks')).filter((tv) => tv.task.course === short && tv.task.kind !== 'post' && tv.task.ai_confirmed !== false);
  return h('section', {},
    h('h1', {}, h('span', { class: 'dot', style: { '--h': hue(short), display: 'inline-block' } }), ' ', c.title),
    tasks.length ? [h('h2', {}, 'Tasques'), h('div', { class: 'cards' }, tasks.map(card))] : null,
    h('h2', {}, 'Continguts'),
    c.sections.map((s, i) => h('details', { open: i < 2 },
      h('summary', {}, s.name),
      s.modules.map((m) => [h('h3', {}, m.name), h('ul', { class: 'list' },
        m.files.filter((f) => !f.is_dir).map((f) => h('li', {}, h('a', { href: `#/fitxers/${enc(f.rel)}` }, f.name))))]))));
};

views.fitxers = async (...parts) => {
  const rel = parts.join('/');
  const info = await api(`/api/files?path=${enc(rel)}`);
  const crumbs = [h('a', { href: '#/fitxers' }, '~/smx')];
  rel.split('/').filter(Boolean).reduce((acc, p) => { const r = acc ? `${acc}/${p}` : p; crumbs.push(' / ', h('a', { href: `#/fitxers/${enc(r)}` }, p)); return r; }, '');
  const head = h('p', { class: 'dim' }, crumbs);
  if (info.kind === 'dir') {
    return h('section', {}, h('h1', {}, 'Fitxers'), head, h('ul', { class: 'list' },
      info.entries.map((e) => h('li', {}, h('a', { href: `#/fitxers/${enc(e.rel)}` }, e.is_dir ? '📁 ' : '', e.name, e.is_dir ? '' : h('span', { class: 'dim' }, `  ${Math.ceil(e.size / 1024)} KB`))))));
  }
  const raw = `/api/raw?path=${enc(rel)}`;
  const name = rel.split('/').pop();
  let body;
  if (info.mime.startsWith('image/')) body = h('img', { src: raw, alt: name, style: { maxWidth: '100%', borderRadius: '16px' } });
  else if (info.mime === 'application/pdf') body = h('iframe', { class: 'body', src: raw, title: 'PDF' });
  else if (info.mime.startsWith('text/html')) body = h('iframe', { class: 'body', src: raw, sandbox: '', title: 'HTML' });
  else if (info.mime.startsWith('text/')) body = h('pre', {}, info.size > 2e6 ? 'Fitxer massa gran per previsualitzar.' : await api(raw));
  else body = h('p', { class: 'dim' }, 'Sense previsualització per a aquest tipus.');
  return h('section', {}, h('h1', {}, name), head,
    h('div', { class: 'actions' }, h('a', { href: raw, download: name }, h('button', { type: 'button' }, 'Descarrega')), fileExtras.map((f) => f(info))),
    body);
};

views.calendari = async (ym) => {
  const all = (await api('/api/tasks')).filter((tv) => tv.task.due && tv.task.kind !== 'post' && tv.task.ai_confirmed !== false);
  const now = new Date();
  const [y, m] = ym ? ym.split('-').map(Number) : [now.getFullYear(), now.getMonth() + 1];
  const first = new Date(y, m - 1, 1), start = new Date(first);
  start.setDate(1 - ((first.getDay() + 6) % 7)); // Monday first
  const key = (d) => `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`;
  const byDay = {};
  for (const tv of all) { const d = new Date(tv.task.due * 1000); (byDay[key(d)] ||= []).push(tv); }
  const cells = [];
  for (let i = 0; i < 42; i++) {
    const d = new Date(start); d.setDate(start.getDate() + i);
    const cls = 'd' + (d.getMonth() !== m - 1 ? ' out' : '') + (key(d) === key(now) ? ' today' : '');
    cells.push(h('div', { class: cls }, h('strong', {}, d.getDate()),
      (byDay[key(d)] || []).map((tv) => h('a', { href: `#/tasca/${enc(tv.task.id)}`, title: tv.task.title },
        h('span', { class: 'dot', style: { '--h': hue(tv.task.course), display: 'inline-block' } }), ' ', isDone(tv) ? '✓ ' : '', tv.task.title))));
  }
  const nav = (dm) => { const d = new Date(y, m - 1 + dm, 1); return `#/calendari/${d.getFullYear()}-${d.getMonth() + 1}`; };
  const title = new Intl.DateTimeFormat('ca-ES', { month: 'long', year: 'numeric' }).format(first);
  return h('section', {}, h('h1', {}, title),
    h('div', { class: 'actions' }, h('a', { href: nav(-1) }, h('button', { type: 'button' }, '← Anterior')), h('a', { href: '#/calendari' }, h('button', { type: 'button' }, 'Avui')), h('a', { href: nav(1) }, h('button', { type: 'button' }, 'Següent →'))),
    h('div', { class: 'cal' }, ['dl', 'dt', 'dc', 'dj', 'dv', 'ds', 'dg'].map((d) => h('div', { class: 'dim' }, d)), cells));
};

views.notes = async () => {
  const gs = await api('/api/grades');
  if (!gs.length) return h('section', {}, h('h1', {}, 'Notes'), h('p', { class: 'dim' }, 'Encara no hi ha notes.'));
  return h('section', {}, h('h1', {}, 'Notes'), gs.map((g) => [
    h('h2', {}, g.course || `Curs ${g.course_id}`),
    h('table', {}, h('thead', {}, h('tr', {}, h('th', {}, 'Element'), h('th', {}, 'Nota'), h('th', {}, '%'))),
      h('tbody', {}, (g.items || []).filter((i) => i.itemname).map((i) => h('tr', {}, h('td', {}, i.itemname), h('td', {}, i.gradeformatted || '—'), h('td', {}, i.percentageformatted || '—')))))]));
};

views.forums = async () => {
  const posts = (await api('/api/tasks')).filter((tv) => tv.task.kind === 'post').sort((a, b) => b.task.opens - a.task.opens);
  const item = (tv) => h('details', { ontoggle: async (e) => {
    const d = e.target;
    if (!d.open || d.dataset.loaded) return;
    d.dataset.loaded = '1';
    const full = await api(`/api/tasks/${enc(tv.task.id)}`);
    d.append(sandboxed(full.task.body_html));
    if (tv.local.status !== 'llegit') {
      await api(`/api/tasks/${enc(tv.task.id)}/status`, { method: 'POST', body: { status: 'llegit' } });
      tv.local.status = 'llegit';
    }
  } }, h('summary', {}, tv.local.status === 'llegit' ? '' : '● ', tv.task.title, h('span', { class: 'dim' }, ` · ${tv.task.course} · ${when(tv.task.opens)}`)));
  const unread = posts.filter((p) => p.local.status !== 'llegit');
  const read = posts.filter((p) => p.local.status === 'llegit');
  return h('section', {}, h('h1', {}, 'Fòrums i avisos'),
    unread.length ? [h('h2', {}, `Sense llegir · ${unread.length}`), unread.map(item)] : h('p', { class: 'dim' }, 'Tot llegit.'),
    read.length ? [h('h2', {}, 'Llegits'), read.map(item)] : null);
};

taskExtras.push((tv) => {
  if (tv.task.kind !== 'assign') return null;
  const id = enc(tv.task.id), files = tv.local.files;
  const up = h('button', { type: 'button', disabled: !files.length, title: files.length ? '' : 'Vincula primer un fitxer', onclick: async () => {
    const body = h('div', {}, h('p', {}, `Tasca: ${tv.task.title} (${tv.task.course})`),
      h('p', {}, 'Es pujaran aquests fitxers com a esborrany. Substitueixen els que ja hi hagi a Moodle:'),
      h('ul', {}, files.map((f) => h('li', {}, f))));
    if (!(await confirmDialog('Puja com a esborrany?', body, 'Puja'))) return;
    try { const r = await api(`/api/tasks/${id}/upload`, { method: 'POST', body: { files } }); toast(`Moodle: ${MSTATUS[r.status] || r.status}`); route(); }
    catch (e) { toast(e.message); }
  } }, 'Puja (esborrany)');
  const send = h('button', { type: 'button', disabled: tv.task.moodle_status !== 'draft', onclick: async () => {
    const body = h('p', {}, `Enviaràs "${tv.task.title}" per qualificar. Després ja no la podràs modificar.`);
    if (!(await confirmDialog('Envia per qualificar?', body, 'Envia'))) return;
    try { const r = await api(`/api/tasks/${id}/submit`, { method: 'POST' }); toast(`Moodle: ${MSTATUS[r.status] || r.status}`); route(); }
    catch (e) { toast(e.message); }
  } }, 'Envia per qualificar');
  return [up, send];
});

const OFFICE = /\.(docx?|odt|rtf|xlsx?|ods|csv|pptx?|odp)$/i;
const editButton = (rel) => h('a', { href: `#/edita/${enc(rel)}` }, h('button', { type: 'button', class: 'primary' }, 'Edita'));
fileExtras.push((info) => (OFFICE.test(info.rel) ? editButton(info.rel) : null));
taskExtras.push((tv) => { const f = tv.local.files.find((x) => OFFICE.test(x)); return f ? editButton(f) : null; });

views.edita = async (...parts) => {
  const rel = parts.join('/');
  const fallback = () => h('div', {}, h('p', { class: 'err' }, 'OnlyOffice no disponible.'),
    h('button', { type: 'button', onclick: () => api('/api/open', { method: 'POST', body: { path: rel } }) }, "Obre amb l'aplicació de l'ordinador"));
  let cfg;
  try { cfg = await api(`/api/office/config?path=${enc(rel)}`); } catch (e) { return fallback(); }
  const holder = h('div', { id: 'oo', style: { height: '80vh' } });
  const wrap = h('section', {}, h('p', {}, h('a', { href: `#/fitxers/${enc(cfg.rel)}` }, '← ', cfg.rel)), holder);
  const s = h('script', { src: cfg.api_js });
  s.onload = () => new window.DocsAPI.DocEditor('oo', cfg.config);
  s.onerror = () => wrap.replaceChildren(fallback());
  document.head.append(s);
  return wrap;
};
