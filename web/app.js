'use strict';
// moodle-web frontend. No build step. Remote HTML only via sandboxed iframes.
const $ = (s, el = document) => el.querySelector(s);
const SVGNS = 'http://www.w3.org/2000/svg';
function ic(name, cls = '') {
  const s = document.createElementNS(SVGNS, 'svg'), u = document.createElementNS(SVGNS, 'use');
  s.setAttribute('class', `ic ${cls}`.trim()); s.setAttribute('aria-hidden', 'true');
  u.setAttribute('href', `#i-${name}`); s.append(u);
  return s;
}

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
  if (!r.ok) {
    const t = await r.text();
    let body; try { body = JSON.parse(t); } catch (e) { body = t; } // 409 JSON bodies drive the IDE dialogs
    throw Object.assign(new Error(t || r.statusText), { status: r.status, body });
  }
  return (r.headers.get('content-type') || '').includes('json') ? r.json() : r.text();
}

const TZ = 'Europe/Madrid';
const FMT = new Intl.DateTimeFormat('ca-ES', { timeZone: TZ, weekday: 'short', day: 'numeric', month: 'numeric', hour: '2-digit', minute: '2-digit', hourCycle: 'h23' });
// '2 oct · 14:30', with weekday 'dj 2 oct · 14:30'. Built from parts so the locale's 'd’oct.' style never shows.
const MON = ['gen', 'febr', 'març', 'abr', 'maig', 'juny', 'jul', 'ag', 'set', 'oct', 'nov', 'des'];
function fmt(d, wk) { const o = {}; for (const p of FMT.formatToParts(d)) o[p.type] = p.value; return `${wk ? o.weekday.replace('.', '') + ' ' : ''}${o.day} ${MON[o.month - 1]} · ${o.hour}:${o.minute}`; }
const fmtHM = new Intl.DateTimeFormat('ca-ES', { timeZone: TZ, hour: '2-digit', minute: '2-digit' });
const syncAt = (ts) => { const d = new Date(ts * 1000); return d.toDateString() === new Date().toDateString() ? fmtHM.format(d) : fmt(d); };
const when = (ts) => (ts ? fmt(new Date(ts * 1000), true) : 'Sense data');
const dk = (d) => d.toLocaleDateString('en-CA', { timeZone: TZ });
function left(ts) {
  if (!ts) return { t: 'Sense venciment', s: 'later' };
  const hrs = (ts * 1000 - Date.now()) / 36e5;
  if (hrs < 0) return { t: 'Endarrerida', s: 'late' };
  if (hrs < 24) return { t: `En ${Math.max(1, Math.round(hrs))} h`, s: 'soon' };
  const d = Math.round((Date.parse(dk(new Date(ts * 1000))) - Date.parse(dk(new Date()))) / 864e5); // calendar days
  if (d <= 1) return { t: 'Demà', s: 'soon' };
  return { t: `En ${d} dies`, s: d < 7 ? 'week' : 'later' };
}
const GLYPH = { late: 'warn', soon: 'clock', done: 'check' };
// Urgency chip: icon + text + colour, never colour alone.
function dueChip(tv, big) {
  const l = isDone(tv) ? { t: 'Lliurada', s: 'done' } : left(tv.task.due);
  return h('span', { class: `due ${l.s}${big ? ' lg' : ''}`, title: tv.task.due ? when(tv.task.due) : null }, GLYPH[l.s] ? ic(GLYPH[l.s]) : null, l.t, tv.task.due ? h('span', { class: 'sr-only' }, ` (${when(tv.task.due)})`) : null);
}
function hue(s) { let x = 0; for (const c of s || '') x = (x * 31 + c.codePointAt(0)) % 360; return x; }
const isDone = (tv) => tv.local.status === 'lliurat' || tv.task.moodle_status === 'submitted';
const KIND = { assign: 'Tasca', quiz: 'Qüestionari', post: 'Avís', ai: 'IA' };
const MSTATUS = { new: 'Sense enviar', draft: 'Esborrany', submitted: 'Enviada' };
const LSTATUS = { 'per revisar': 'Per revisar', 'revisat OK': 'Revisada', lliurat: 'Lliurada' };
const aiBadge = () => h('span', { class: 'badge ai' }, 'IA');
let COURSES = {}; // shortname -> title
const cname = (short) => COURSES[short] || short;
const setText = (el, v) => { if (el.textContent !== v) el.textContent = v; }; // live regions re-announce on every write
const enc = encodeURIComponent;

function sandboxed(html, label) {
  if (!html || !html.trim()) return h('p', { class: 'dim' }, 'Sense descripció a Moodle.');
  const c = getComputedStyle(document.documentElement), v = (n) => c.getPropertyValue(n).trim();
  const SERIF = v('--serif'), MONO = v('--mono'), INK = v('--text'), ACC = v('--accent'), RULE = v('--line'), DIM = v('--dim'), SUNK = v('--sunk');
  const css = `<style>:root{color-scheme:${c.colorScheme}}html{background:transparent;color:${INK};font:18px/1.7 ${SERIF}}body{max-width:68ch;margin:0 auto;padding:24px 16px;overflow-wrap:anywhere}a{color:${ACC};text-underline-offset:.2em}img,video{max-width:100%;height:auto}table{border-collapse:collapse;width:100%}td,th{border-bottom:1px solid ${RULE};padding:8px 16px;text-align:left}blockquote{border-left:3px solid ${ACC};margin:0;padding-left:16px;color:${DIM}}pre,code{font-family:${MONO};background:${SUNK}}h1,h2,h3,h4{font-family:${SERIF};line-height:1.3}*{max-width:100%}font,[style*=color]{color:inherit!important}[style*=background],[bgcolor]{background:transparent!important}a{color:${ACC}!important}</style>`;
  return h('iframe', { class: 'sheet', sandbox: '', referrerpolicy: 'no-referrer', title: label || 'Contingut de Moodle', srcdoc: css + html });
}

function toast(msg, bad) {
  const t = $('#toast'), ok = $('#toast-s'), er = $('#toast-e'), x = $('#toast-x');
  clearTimeout(toast.t); clearTimeout(toast.c); clearTimeout(toast.h);
  ok.textContent = er.textContent = '';
  // Clear first, set on the next tick: repeated messages get announced again.
  toast.t = setTimeout(() => {
    (bad ? er : ok).textContent = msg;
    t.classList.add('on'); t.classList.toggle('bad', !!bad); x.hidden = !bad;
    if (!bad) toast.h = setTimeout(hideToast, toast.ms = msg.length > 60 ? 6000 : 3000);
  }, 50);
}
// Keep the live regions in the tree (sr-only when empty); clear the text once the fade is done.
function hideToast() {
  $('#toast').classList.remove('on'); $('#toast-x').hidden = true;
  toast.c = setTimeout(() => { $('#toast-s').textContent = $('#toast-e').textContent = ''; }, 250);
}
$('#toast-x').addEventListener('click', hideToast);
// Success toasts pause while hovered or focused.
$('#toast').addEventListener('pointerenter', () => clearTimeout(toast.h));
$('#toast').addEventListener('focusin', () => clearTimeout(toast.h));
$('#toast').addEventListener('pointerleave', () => { if ($('#toast').classList.contains('on') && !$('#toast').classList.contains('bad')) toast.h = setTimeout(hideToast, toast.ms || 3000); });

// Shared states. All text via h()/textContent.
const empty = (title, text, btn) => h('div', { class: 'empty' }, h('h2', {}, title), text && h('p', {}, text), btn);
const errorPanel = (msg) => h('div', { class: 'err-panel', role: 'alert' }, ic('warn'), h('p', {}, msg), h('button', { type: 'button', class: 'ghost', onclick: () => route() }, 'Torna-ho a provar'));
const skeleton = () => h('div', {}, h('span', { class: 'sr-only' }, 'Carregant…'), [1, 2, 3].map(() => h('div', { class: 'skel' })));
function bar(n, m, label, cls = '', text = `${n}/${m}`) {
  return h('span', { class: 'barwrap' },
    h('span', { class: `bar ${cls}`, role: 'progressbar', 'aria-valuemin': 0, 'aria-valuemax': m, 'aria-valuenow': n, 'aria-label': label }, h('i', { style: { width: `${m ? Math.round((100 * n) / m) : 0}%` } })),
    h('span', { class: 'num-t' }, text));
}

function confirmDialog(title, body, okLabel = "D'acord", info = false) {
  const d = $('#dlg');
  $('#dlg menu button[value=no]').hidden = info;
  $('#dlg-t').textContent = title;
  $('#dlg-b').replaceChildren(body);
  const p1 = body.querySelector?.('p'); // announce only the first paragraph, not long file lists
  if (p1) { p1.id = 'dlg-d'; d.setAttribute('aria-describedby', 'dlg-d'); } else d.removeAttribute('aria-describedby');
  $('#dlg-ok').textContent = okLabel;
  d.returnValue = '';
  d.showModal();
  return new Promise((res) => d.addEventListener('close', () => res(d.returnValue === 'ok'), { once: true }));
}

const views = {};
const taskExtras = [];
const fileExtras = [];

function card(tv, hideCourse) {
  const t = tv.task, done = isDone(tv);
  // Badges only when they differ from the default 'Sense enviar'.
  const badges = [
    t.moodle_status !== 'new' && h('span', { class: 'badge' + (t.moodle_status === 'submitted' ? ' ok' : '') }, MSTATUS[t.moodle_status] || t.moodle_status),
    tv.local.status && h('span', { class: 'badge ' + (tv.local.status === 'revisat OK' ? 'ok' : 'warn') }, LSTATUS[tv.local.status] || tv.local.status),
    t.source === 'ai' && aiBadge(),
  ];
  return h('a', { class: 'row' + (done ? ' done' : left(t.due).s === 'late' ? ' late' : ''), href: `#/tasca/${enc(t.id)}` },
    h('span', { class: 'dot', style: { '--h': hue(t.course) } }),
    h('span', { class: 't', title: t.title }, t.title),
    h('span', { class: 'm' }, hideCourse ? null : h('span', { class: 'cn', title: cname(t.course), style: { '--h': hue(t.course) } }, cname(t.course)), h('span', {}, KIND[t.kind] || t.kind), badges),
    dueChip(tv));
}

// AI scan: one poller shared by every scanControl() instance (sidebar + tauler).
function scanTotals(p) {
  const total = Object.values(p.counts || {}).reduce((a, n) => a + n, 0);
  return { total, finished: total - ((p.counts || {}).pending || 0) };
}
const evidenceOf = (html) => new DOMParser().parseFromString(html || '', 'text/html').querySelector('blockquote')?.textContent.trim() || '';
const SCAN = { nodes: new Set(), p: null, timer: 0 };
async function scanPoll() {
  let p;
  try { p = await api('/api/scan'); } catch (e) { return; }
  const was = SCAN.p?.state;
  SCAN.p = p;
  for (const n of SCAN.nodes) { if (n.seen && !n.isConnected) SCAN.nodes.delete(n); else { n.seen = true; n.render(p); } }
  if (p.state === 'running') { SCAN.timer ||= setInterval(scanPoll, 2000); return; }
  clearInterval(SCAN.timer); SCAN.timer = 0;
  if (was !== 'running') return;
  const c = p.counts || {};
  setText($('#live'), p.state === 'done' ? `Escaneig acabat: ${p.proposed} proposades, ${c.needs_review || 0} per revisar, ${c.error || 0} errors` : p.state === 'cancelled' ? 'Escaneig cancel·lat' : "Escaneig interromput");
  if (route.cur === 'tauler') route({ soft: true });
}
function scanControl() {
  const SL = { needs_review: 'Per revisar', error: 'Error' };
  const go = h('button', { type: 'button', class: 'primary', onclick: async () => {
    if (SCAN.p?.state === 'running') return;
    try { await api('/api/scan', { method: 'POST' }); } catch (e) { if (e.status === 409) toast('Ja hi ha un escaneig en marxa'); else return toast(e.message, true); }
    scanPoll();
  } }, ic('search'), 'Escaneja Moodle amb IA');
  const stop = h('button', { type: 'button', class: 'ghost', hidden: true, onclick: async () => { try { await api('/api/scan/cancel', { method: 'POST' }); } catch (e) { toast(e.message, true); } scanPoll(); } }, 'Cancel·la');
  const prog = h('div', { class: 'v-scan-p' }), list = h('ul', { class: 'v-scan-l' });
  const det = h('details', { hidden: true }, h('summary', {}, 'Elements per revisar'), list);
  const node = h('section', { class: 'v-scan', 'aria-label': 'Escaneig amb IA' }, h('div', { class: 'field-row' }, go, stop), prog, det);
  node.render = (p) => {
    const run = p.state === 'running', { finished, total } = scanTotals(p);
    go.setAttribute('aria-disabled', String(run)); stop.hidden = !run;
    prog.replaceChildren(...(run ? [bar(finished, total, "Progrés de l'escaneig", '', `${finished} de ${total} fitxers`)]
      : p.state === 'idle' ? [] : [h('p', { class: 'dim' }, `${{ done: 'Acabat', cancelled: 'Cancel·lat', interrupted: 'Interromput' }[p.state] || p.state}: ${p.proposed || 0} proposades`)]));
    list.replaceChildren(...(p.items || []).map((i) => h('li', {}, h('b', {}, SL[i.status] || i.status), ` ${i.course} · `, h('span', { class: 'mono' }, i.src), i.error ? ` · ${i.error}` : '')));
    det.hidden = !(p.items || []).length;
  };
  SCAN.nodes.add(node);
  if (SCAN.p) node.render(SCAN.p);
  scanPoll();
  return node;
}
// "Proposades per IA": evidence is text only, preview loads on first open.
function proposal(tv) {
  const t = tv.task, box = h('div', { class: 'v-preview' }), web = /^https?:\/\//.test(t.url);
  const act = (keep, label) => h('button', { type: 'button', class: keep ? 'primary' : '', onclick: async () => {
    try { await api(`/api/tasks/${enc(t.id)}/ai`, { method: 'POST', body: { keep } }); setText($('#live'), keep ? `Mantinguda: ${t.title}` : `Descartada: ${t.title}`); route({ soft: true }); } catch (e) { toast(e.message, true); } } }, label);
  return h('article', { class: 'v-prop' },
    h('h3', {}, h('a', { href: `#/tasca/${enc(t.id)}` }, t.title)),
    h('p', { class: 'm' }, h('span', { class: 'cn', style: { '--h': hue(t.course) } }, cname(t.course)), t.due ? ` · ${when(t.due)}` : null),
    evidenceOf(t.body_html) ? h('blockquote', {}, evidenceOf(t.body_html)) : null,
    h('p', { class: 'dim' }, 'Font: ', web ? h('a', { href: t.url, target: '_blank', rel: 'noopener' }, t.url, ic('ext'), h('span', { class: 'sr-only' }, ' (obre en una pestanya nova)')) : h('a', { href: `#/fitxers/${enc(t.url)}` }, t.url)),
    h('details', { ontoggle: (e) => { if (e.target.open && !box.hasChildNodes()) preview(box, web ? { moodleUrl: t.url } : { path: t.url }); } }, h('summary', {}, 'Previsualitza'), box),
    h('div', { class: 'field-row' }, act(true, 'Mantén'), act(false, 'Descarta')));
}

views.tauler = async () => {
  const tasks = (await api('/api/tasks')).filter((tv) => tv.task.kind !== 'post' && tv.task.ai_confirmed !== false);
  const props = tasks.filter((tv) => tv.task.source === 'ai' && tv.task.ai_confirmed == null), all = tasks.filter((tv) => !props.includes(tv));
  const now = Date.now() / 1000, week = now + 7 * 86400;
  // Overdue by more than 60 days = last school year's leftovers: collapsed, not counted as pending.
  const stale = (tv) => !isDone(tv) && tv.task.due && tv.task.due < now - 60 * 86400;
  const groups = [
    ['late', 'Endarrerides', (tv) => !isDone(tv) && !stale(tv) && tv.task.due && tv.task.due < now],
    ['week', 'Aquesta setmana', (tv) => !isDone(tv) && tv.task.due >= now && tv.task.due <= week],
    ['later', 'Més endavant', (tv) => !isDone(tv) && (!tv.task.due || tv.task.due > week)],
    ['done', 'Lliurades', isDone],
  ];
  const pendingList = all.filter((tv) => !isDone(tv) && !stale(tv));
  if (!all.length && !props.length) return h('section', { class: 'v-tauler' }, h('h1', {}, 'Tauler'), scanControl(), empty('Encara no hi ha tasques', 'Sincronitza per carregar-les.'));
  const pending = pendingList.length;
  const old = all.filter(stale);
  const doneN = all.filter(isDone).length, total = all.length - old.length;
  const next = pendingList.filter((tv) => tv.task.due >= now).sort((a, b) => a.task.due - b.task.due)[0];

  // Each block: { key, el, rows[] }. Filtering only toggles `hidden` on what is already rendered.
  const blocks = [];
  const block = (key, el, rows) => { blocks.push({ key, el, rows }); return el; };
  const rowsOf = (items) => items.map((tv) => { const r = card(tv); return r; });
  const grp = (key, name, items, late) => {
    const rows = rowsOf(items);
    return block(key, h('section', { class: `v-grp g-${key}` },
      h('h2', { class: 'grp' }, name, h('span', { class: 'n' }, items.length)),
      h('div', { class: 'cards' }, rows)), rows);
  };
  const fold = (key, name, items, open) => {
    const rows = rowsOf(items);
    return block(key, h('details', { class: `fold g-${key}`, open }, h('summary', { class: 'grp' }, h('h2', {}, name, h('span', { class: 'n' }, items.length))), h('div', { class: 'cards' }, rows)), rows);
  };

  const sections = groups.map(([key, name, f]) => {
    const items = all.filter(f);
    if (key === 'done') items.reverse();
    if (!items.length) return null;
    return key === 'done' ? fold(key, name, items, items.length <= 5) : grp(key, name, items, key === 'late');
  });
  const oldEl = old.length ? fold('old', 'Antigues sense enviar', old, false) : null;

  const status = h('p', { class: 'sr-only', role: 'status' });
  const none = h('div', { hidden: true }, empty('Cap tasca coincideix', 'Prova un altre filtre o esborra la cerca.'));
  let mode = 'all';
  const apply = () => {
    const q = input.value.trim().toLowerCase();
    let shown = 0;
    for (const b of blocks) {
      // 'Antigues' counts as overdue for the segmented filter.
      const inMode = mode === 'all' || mode === b.key || (mode === 'late' && b.key === 'old');
      let n = 0;
      for (const r of b.rows) { const ok = inMode && (!q || r.textContent.toLowerCase().includes(q)); r.hidden = !ok; if (ok) n++; }
      b.el.hidden = !n;
      if (n && b.el.tagName === 'DETAILS' && (q || mode !== 'all')) b.el.open = true;
      shown += n;
    }
    none.hidden = shown > 0;
    status.textContent = `${shown} ${shown === 1 ? 'tasca' : 'tasques'}`;
  };
  const input = h('input', { id: 'filter', type: 'search', 'aria-label': 'Filtra tasques', placeholder: 'Filtra tasques  ( / )', title: 'Drecera: / enfoca, Esc esborra', value: views.tauler.q || '', oninput: () => { views.tauler.q = input.value; apply(); } });
  const FILTERS = [['all', 'Totes'], ['late', 'Endarrerides'], ['week', 'Aquesta setmana'], ['later', 'Més endavant'], ['done', 'Lliurades']];
  const seg = h('div', { class: 'seg', role: 'group', 'aria-label': 'Filtre per estat' }, FILTERS.map(([k, label]) =>
    h('button', { type: 'button', 'aria-pressed': String(k === mode), onclick: (e) => {
      mode = k; seg.querySelectorAll('button').forEach((b) => b.setAttribute('aria-pressed', b === e.currentTarget)); apply();
    } }, label)));

  const out = h('section', { class: 'v-tauler' },
    h('h1', {}, pending === 0 ? 'Tot al dia' : `Tens ${pending} ${pending === 1 ? 'tasca pendent' : 'tasques pendents'}`),
    h('div', { class: 'sub' },
      next ? h('span', {}, 'La propera: ', h('a', { href: `#/tasca/${enc(next.task.id)}` }, next.task.title)) : null,
      total ? bar(doneN, total, 'Tasques lliurades', '', `${doneN} de ${total} lliurades`) : null),
    all.length ? h('div', { class: 'v-tools' }, seg, h('label', { class: 'sr-only', for: 'filter' }, 'Filtra tasques'), input) : null,
    scanControl(), status,
    props.length ? h('section', { class: 'v-grp g-ai' }, h('h2', { class: 'grp' }, 'Proposades per IA', h('span', { class: 'n' }, props.length)), h('div', { class: 'v-props' }, props.map(proposal))) : null,
    sections, oldEl, none);
  if (input.value) apply(); // filter survives soft refreshes
  return out;
};

const fsize = (n) => (typeof n !== 'number' ? '' : n < 1024 ? `${n} B` : n < 1048576 ? `${Math.ceil(n / 1024)} KB` : `${(n / 1048576).toFixed(1)} MB`);

// preview(el, {path|moodleUrl, name, size}): fills el with the file. Never innerHTML; html/svg stay sandboxed.
const EXT = {
  img: 'png jpg jpeg gif webp', pdf: 'pdf', html: 'html htm svg', video: 'mp4 webm ogv m4v mov', audio: 'mp3 wav ogg m4a flac', zip: 'zip',
  office: 'doc docx odt rtf ppt pptx odp xls xlsx ods', text: 'txt md csv json xml css js ts rs py sql url sh java toml yaml yml c h cpp php log ini',
};
const kindOf = (n) => { const e = (n.split(/[?#]/)[0].split('.').pop() || '').toLowerCase(); return Object.keys(EXT).find((k) => EXT[k].split(' ').includes(e)); };
// Names and sizes from the zip central directory (no library, no inflate).
function zipEntries(buf) {
  const v = new DataView(buf), td = new TextDecoder(), out = [];
  let i = buf.byteLength - 22;
  while (i >= 0 && v.getUint32(i, true) !== 0x06054b50) i--;
  if (i < 0) return out;
  let p = v.getUint32(i + 16, true);
  for (let n = v.getUint16(i + 10, true); n-- > 0 && v.getUint32(p, true) === 0x02014b50;) {
    const nl = v.getUint16(p + 28, true), skip = nl + v.getUint16(p + 30, true) + v.getUint16(p + 32, true);
    out.push({ name: td.decode(new Uint8Array(buf, p + 46, nl)), size: v.getUint32(p + 24, true) });
    p += 46 + skip;
  }
  return out;
}
async function preview(el, o) {
  const name = o.name || (o.path || o.moodleUrl.split(/[?#]/)[0]).split('/').pop();
  const src = o.path != null ? `/api/raw?path=${enc(o.path)}` : `/api/moodle-file?url=${enc(o.moodleUrl)}`;
  const k = kindOf(name), big = o.size > 2e6;
  const pv = `/api/preview?${o.path != null ? 'path' : 'moodle'}=${enc(o.path ?? o.moodleUrl)}`; // office -> PDF, .url -> target + scanner text
  const fail = (t, b) => h('div', { class: 'pv-none' }, h('p', {}, t), b);
  const dl = h('a', { class: 'btn', href: src, download: name }, ic('file'), 'Descarrega');
  let node;
  try {
    if (k === 'img') node = h('img', { class: 'v-img', src, alt: name });
    else if (k === 'pdf') node = h('iframe', { class: 'sheet v-frame', src, title: `PDF: ${name}` });
    else if (k === 'html') node = h('iframe', { class: 'sheet v-frame', src, sandbox: '', referrerpolicy: 'no-referrer', title: `Previsualització: ${name}` });
    else if (k === 'video') node = h('video', { class: 'v-img', src, controls: true, 'aria-label': name });
    else if (k === 'audio') node = h('audio', { src, controls: true, 'aria-label': name });
    else if (k === 'office') node = h('iframe', { class: 'sheet v-frame', src: pv, title: `PDF: ${name}` });
    else if (k === 'text' && !big) node = h('pre', { tabindex: 0, role: 'region', 'aria-label': `Contingut de ${name}` }, (await api(name.toLowerCase().endsWith('.url') ? pv : src)).slice(0, 2e5));
    else if (k === 'zip' && !big) {
      const es = zipEntries(await (await fetch(src)).arrayBuffer());
      node = es.length ? h('div', { class: 'tablewrap', role: 'region', tabindex: 0, 'aria-label': `Contingut de ${name}` },
        h('table', {}, h('caption', { class: 'sr-only' }, `Entrades de ${name}`),
          h('thead', {}, h('tr', {}, h('th', { scope: 'col' }, 'Nom'), h('th', { scope: 'col', class: 'num' }, 'Mida'))),
          h('tbody', {}, es.map((e) => h('tr', {}, h('td', { class: 'mono' }, e.name), h('td', { class: 'num' }, e.name.endsWith('/') ? '' : fsize(e.size))))))) : fail('Zip buit o il·legible.', dl);
    } else node = fail(big ? 'Fitxer massa gran per previsualitzar.' : "Aquest tipus de fitxer no es pot previsualitzar. Descarrega'l per obrir-lo.", dl);
  } catch (e) { node = fail(`No s'ha pogut carregar: ${e.message}`, dl); }
  el.replaceChildren(node);
}

// WAI-ARIA tabs; panels render once on first select; active id survives soft refreshes in tabs.sel[key].
function tabs(key, items) {
  tabs.sel ??= {};
  const btns = [], panels = [];
  const pick = (i, focus) => {
    tabs.sel[key] = items[i].id;
    items.forEach((it, j) => {
      btns[j].setAttribute('aria-selected', String(j === i)); btns[j].tabIndex = j === i ? 0 : -1; panels[j].hidden = j !== i;
    });
    if (!panels[i].hasChildNodes()) panels[i].append(items[i].render());
    if (focus) btns[i].focus();
  };
  items.forEach((it, i) => {
    btns.push(h('button', { type: 'button', role: 'tab', id: `tab-${key}-${it.id}`, 'aria-controls': `pn-${key}-${it.id}`, onclick: () => pick(i),
      onkeydown: (e) => {
        const n = { ArrowRight: (i + 1) % items.length, ArrowLeft: (i + items.length - 1) % items.length, Home: 0, End: items.length - 1 }[e.key];
        if (n != null) { e.preventDefault(); pick(n, true); }
      } }, it.label));
    panels.push(h('div', { role: 'tabpanel', id: `pn-${key}-${it.id}`, 'aria-labelledby': `tab-${key}-${it.id}`, tabindex: 0, hidden: true }));
  });
  pick(Math.max(0, items.findIndex((it) => it.id === tabs.sel[key])));
  return h('div', { class: 'tabs2' }, h('div', { role: 'tablist', 'aria-label': 'Seccions de la tasca' }, btns), panels);
}

views.tasca = async (id) => {
  const tv = await api(`/api/tasks/${enc(id)}`);
  const t = tv.task, l = tv.local;
  // Save only on an explicit click: arrow keys on a closed <select> fire `change` per step,
  // and 'lliurat' moves the submit folder, so it also needs a confirmation.
  const select = h('select', { id: 'estat-sel', 'aria-label': 'Estat local' },
    ['', 'per revisar', 'revisat OK', 'lliurat'].map((s) => h('option', { value: s, selected: s === l.status }, s ? LSTATUS[s] : '— sense estat —')));
  const selErr = h('p', { class: 'err', id: 'estat-err', role: 'alert' });
  select.setAttribute('aria-describedby', 'estat-err');
  const save = h('button', { type: 'button', id: 'estat-desa', onclick: async () => {
    const next = select.value;
    if (next === l.status) { selErr.textContent = 'Cap canvi per desar.'; select.setAttribute('aria-invalid', 'true'); return; }
    if (next === 'lliurat' && !(await confirmDialog('Marcar com a lliurada?',
      h('p', {}, `La carpeta de "${t.title}" es mourà a submit/_lliurat/.`), 'Marca lliurada'))) { select.value = l.status; save.focus(); return; }
    try { await api(`/api/tasks/${enc(id)}/status`, { method: 'POST', body: { status: next } }); selErr.textContent = ''; select.removeAttribute('aria-invalid'); toast('Estat desat'); await route({ soft: true }); $('#estat-desa')?.focus(); }
    catch (err) { selErr.textContent = err.message; select.setAttribute('aria-invalid', 'true'); select.value = l.status; }
  } }, 'Desa estat');
  const aiBtn = (bid, label, keep, after) => h('button', { type: 'button', id: bid, onclick: async () => {
    try { await api(`/api/tasks/${enc(id)}/ai`, { method: 'POST', body: { keep } }); after(); } catch (e) { toast(e.message, true); } } }, label);
  const extras = taskExtras.map((f) => f(tv)).reverse(); // Edita first, then Puja / Envia
  const NEWTAB = h('span', { class: 'sr-only' }, ' (obre en una pestanya nova)');

  const rail = h('section', { class: 'rail', 'aria-labelledby': 'rail-t' },
    h('h2', { class: 'eyebrow', id: 'rail-t' }, 'Detalls'),
    h('dl', { class: 'kv' },
      h('div', {}, h('dt', {}, 'Venciment'), h('dd', {}, when(t.due))),
      t.cutoff ? h('div', {}, h('dt', {}, 'Tancament'), h('dd', {}, when(t.cutoff))) : null,
      h('div', {}, h('dt', {}, 'Moodle'), h('dd', {}, MSTATUS[t.moodle_status] || t.moodle_status)),
      t.grade ? h('div', {}, h('dt', {}, 'Nota'), h('dd', {}, t.grade)) : null),
    t.source === 'ai' && t.ai_confirmed == null && h('div', { class: 'notice', role: 'group', 'aria-label': 'Tasca extreta per IA' },
      h('p', {}, 'Extreta per IA. Revisa-la i decideix si la mantens.'),
      h('div', { class: 'field-row' }, aiBtn('ai-keep', 'Mantén', true, () => route({ soft: true })), aiBtn('ai-drop', 'Descarta', false, () => { location.hash = '#/tauler'; }))),
    h('a', { class: 'ext', href: t.url, target: '_blank', rel: 'noopener' }, 'Obre a Moodle', ic('ext'), NEWTAB),
    h('div', { class: 'fld' }, h('label', { for: 'estat-sel' }, 'Estat local'), h('div', { class: 'field-row' }, select, save), selErr),
    extras.flat(Infinity).some(Boolean) ? h('div', { class: 'v-deliver' }, h('h2', { class: 'sr-only' }, 'Entrega'), extras) : null);

  const badges = [
    l.status && h('span', { class: 'badge ' + (l.status === 'revisat OK' ? 'ok' : 'warn') }, LSTATUS[l.status] || l.status),
    t.source === 'ai' && aiBadge()];

  const intro = sandboxed(t.body_html, `Enunciat: ${t.title}`);
  const fb = t.feedback_html ? sandboxed(t.feedback_html, 'Retroacció') : null;
  if (fb && fb.tagName === 'IFRAME') fb.classList.add('fb');
  const attRows = t.attachments.map((a) => {
    const box = h('div', { class: 'v-preview' });
    return h('details', { class: 'att', ontoggle: (e) => { if (e.target.open && !box.hasChildNodes()) preview(box, { moodleUrl: a.fileurl, name: a.filename, size: a.filesize }); } },
      h('summary', { class: 'row v-file' }, ic('file'), h('span', { class: 't' }, a.filename), a.filesize != null && h('span', { class: 'mono dim' }, fsize(a.filesize))),
      box, h('a', { class: 'btn ghost', href: `/api/moodle-file?url=${enc(a.fileurl)}`, download: a.filename }, 'Descarrega'));
  });
  const shotUrl = () => `/api/shot/${enc(id)}?t=${Date.now()}`;
  const enunciat = () => {
    const slot = h('div', { class: 'v-shotbox' });
    const showText = () => slot.replaceChildren(h('p', { class: 'dim' }, 'Sense captura.'), intro);
    const showShot = () => { const img = h('img', { class: 'v-shot', src: shotUrl(), alt: "Captura de l'enunciat" }); img.onerror = showText; slot.replaceChildren(img); };
    const snap = h('button', { type: 'button', onclick: async () => {
      snap.setAttribute('aria-busy', 'true'); snap.disabled = true;
      try { await api(`/api/shot/${enc(id)}`, { method: 'POST' }); showShot(); toast('Captura feta'); }
      catch (e) { toast(e.message, true); }
      snap.removeAttribute('aria-busy'); snap.disabled = false;
    } }, 'Fes captura');
    showShot();
    return h('div', { class: 'v-pane' }, h('div', { class: 'field-row' }, snap), slot,
      attRows.length ? [h('h2', { class: 'eyebrow' }, 'Adjunts del professor'), h('div', { class: 'list' }, attRows)] : null,
      fb ? [h('h2', { class: 'eyebrow' }, 'Retroacció'), fb] : null);
  };
  const feina = () => h('div', { class: 'v-pane' },
    l.files.length ? l.files.map((f) => {
      const box = h('div', { class: 'v-preview' });
      preview(box, { path: f });
      return h('section', { class: 'v-item', 'aria-label': f },
        h('h3', { class: 'mono' }, f), h('div', { class: 'field-row' }, fileExtras.map((x) => x({ rel: f }))), box);
    }) : h('p', { class: 'dim' }, `Cap fitxer vinculat. Carpeta suggerida: ${tv.submit_dir}`),
    linkFiles(id, l.files));
  const lliurat = () => h('div', { class: 'v-pane' },
    (t.submission_files || []).length ? t.submission_files.map((f) => {
      const box = h('div', { class: 'v-preview' });
      preview(box, { moodleUrl: f.fileurl, name: f.filename, size: f.filesize });
      return h('section', { class: 'v-item', 'aria-label': f.filename },
        h('h3', {}, f.filename, ' ', h('span', { class: 'mono dim' }, fsize(f.filesize))),
        h('div', { class: 'field-row' }, h('a', { class: 'btn ghost', href: `/api/moodle-file?url=${enc(f.fileurl)}`, download: f.filename }, 'Descarrega')), box);
    }) : empty('Res lliurat', "Encara no hi ha cap fitxer entregat a Moodle."));

  const main = h('div', { class: 'v-main' }, tabs('tasca', [
    { id: 'enunciat', label: 'Enunciat', render: enunciat },
    { id: 'feina', label: 'La meva feina', render: feina },
    { id: 'lliurat', label: 'Lliurat a Moodle', render: lliurat }]));

  return h('article', { class: 'v-tasca' },
    h('a', { class: 'btn ghost', href: '#/tauler' }, ic('arrow-l'), 'Tauler'),
    h('h1', {}, t.title),
    h('div', { class: 'meta' },
      h('a', { class: 'chip', href: `#/curs/${enc(t.course)}` }, h('span', { class: 'dot', style: { '--h': hue(t.course) } }), cname(t.course)),
      dueChip(tv, true), badges),
    h('div', { class: 'split' }, h('div', { class: 'grid' }, main, rail)));
};

function linkFiles(id, current) {
  const err = h('p', { class: 'err', id: 'vincula-err', role: 'alert' });
  const input = h('input', { id: 'vincula-in', list: 'submit-files', placeholder: 'submit/…/fitxer_amin.docx', 'aria-describedby': 'vincula-err' });
  const dl = h('datalist', { id: 'submit-files' });
  api('/api/submit-files').then((fs) => dl.replaceChildren(...fs.map((f) => h('option', { value: f })))).catch((e) => toast(e.message, true));
  return h('div', { class: 'fld v-link' }, h('label', { for: 'vincula-in' }, 'Vincula un fitxer'),
    h('div', { class: 'field-row' }, input, dl, h('button', { type: 'button', id: 'vincula', onclick: async () => {
      if (!input.value) { err.textContent = 'Escriu o tria un fitxer.'; input.setAttribute('aria-invalid', 'true'); input.focus(); return; }
      err.textContent = ''; input.removeAttribute('aria-invalid');
      try { await api(`/api/tasks/${enc(id)}/files`, { method: 'POST', body: { files: [...current, input.value] } }); await route({ soft: true }); $('#vincula')?.focus(); }
      catch (e) { err.textContent = e.message; input.setAttribute('aria-invalid', 'true'); }
    } }, 'Vincula')), err);
}

function markNav() {
  const [name, arg] = (location.hash.slice(2) || 'tauler').split('/'), nm = views[name] ? name : 'tauler';
  document.querySelectorAll('.tabs a').forEach((a) => (a.getAttribute('href') === `#/${nm}` ? a.setAttribute('aria-current', 'page') : a.removeAttribute('aria-current')));
  document.querySelectorAll('#courses a').forEach((a) => (nm === 'curs' && a.getAttribute('href') === `#/curs/${arg}` ? a.setAttribute('aria-current', 'page') : a.removeAttribute('aria-current')));
}
// route({soft:true}): refresh after an action. Keeps the old view until the new one is ready,
// restores scroll and never shows the skeleton. Plain route(): real navigation.
// Scrollable table wrappers get keyboard focus only while they overflow.
const wrapFocus = () => document.querySelectorAll('#main .tablewrap').forEach((w) => { if (w.scrollHeight > w.clientHeight || w.scrollWidth > w.clientWidth) w.tabIndex = 0; else w.removeAttribute('tabindex'); });
addEventListener('resize', wrapFocus);
async function route(o) {
  const soft = o?.soft === true;
  if (!soft) delete views.tauler.q;
  const my = (route.n = (route.n || 0) + 1);
  const [name, ...rest] = (location.hash.slice(2) || 'tauler').split('/');
  const nm = views[name] ? name : 'tauler';
  const main = $('#main');
  const y = scrollY, first = !route.cur, same = route.cur === nm;
  const had = main.contains(document.activeElement), aid = had ? document.activeElement.id : '';
  markNav();
  main.setAttribute('aria-busy', 'true');
  if (!soft) { main.dataset.view = nm; main.classList.toggle('edit', nm === 'edita' || nm === 'ide'); main.replaceChildren(skeleton()); }
  let node;
  try { node = await views[nm](...rest.map(decodeURIComponent)); } catch (e) { node = errorPanel(e.message); }
  if (my !== route.n) return; // a newer navigation won
  route.cur = nm;
  const paint = () => {
    main.dataset.view = nm; main.classList.toggle('edit', nm === 'edita' || nm === 'ide');
    main.replaceChildren(node); main.removeAttribute('aria-busy');
    node.dispatchEvent?.(new Event('mounted'));
    const h1 = main.querySelector('h1');
    document.title = `${h1 ? h1.textContent.trim() + ' · ' : ''}Institut`;
    if (!soft && h1 && (first || same)) setText($('#live'), h1.textContent.trim()); // announced only when the h1 is not focused
    wrapFocus();
    if (soft) scrollTo(0, y); else if (!first) scrollTo(0, 0);
    const back = aid && document.getElementById(aid);
    if (back && !back.disabled) back.focus({ preventScroll: true });
    else if (h1 && (soft ? had : !first && !same)) { h1.tabIndex = -1; h1.focus({ preventScroll: true }); }
  };
  document.startViewTransition && !soft ? document.startViewTransition(paint) : paint();
  forumPill();
}

// Unread forum posts -> nav pill (hidden at 0).
async function forumPill() {
  try {
    const n = (await api('/api/tasks')).filter((tv) => tv.task.kind === 'post' && tv.local.status !== 'llegit').length;
    const p = $('#forum-pill'); p.hidden = !n; p.replaceChildren(String(n), h('span', { class: 'sr-only' }, ' sense llegir')); p.title = `${n} sense llegir`;
  } catch (e) { /* ignore */ }
}

async function refreshStatus() {
  try {
    const s = await api('/api/status');
    setText($('#lastsync'), s.syncing ? 'Sincronitzant…' : s.last_sync ? `Sincronitzat ${syncAt(s.last_sync)}` : 'Mai sincronitzat');
    const b = $('#banner'); b.hidden = !s.error; setText(b, s.error || '');
  } catch (e) { /* server restarting */ }
}

async function loadCourses() {
  const cs = await api('/api/courses');
  COURSES = Object.fromEntries(cs.map((c) => [c.shortname, c.title]));
  $('#courses').hidden = $('.side .sec').hidden = !cs.length;
  $('#courses').replaceChildren(...cs.map((c) => h('a', { href: `#/curs/${enc(c.shortname)}`, title: c.title }, h('span', { class: 'dot', style: { '--h': hue(c.shortname) } }), h('span', { class: 'nm' }, c.title))));
  markNav();
}

$('#scan-slot').append(scanControl());
$('#sync').addEventListener('click', async (e) => {
  const b = e.currentTarget;
  if (b.getAttribute('aria-busy') === 'true') return;
  b.setAttribute('aria-disabled', 'true'); b.setAttribute('aria-busy', 'true');
  $('#lastsync').textContent = 'Sincronitzant…';
  try { const r = await api('/api/sync', { method: 'POST' }); toast(`Sincronitzat (${r.events} canvis)`); await route({ soft: true }); }
  catch (err) { toast(err.message, true); }
  b.removeAttribute('aria-disabled'); b.removeAttribute('aria-busy'); refreshStatus();
});
const curTheme = () => document.documentElement.dataset.theme || (matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark');
// Sandboxed iframes bake the theme colours into srcdoc, so a theme change re-renders the view.
function themeChanged() { $('#theme-use').setAttribute('href', curTheme() === 'light' ? '#i-sun' : '#i-moon'); $('#theme').setAttribute('aria-pressed', String(curTheme() === 'dark')); window.monaco?.editor.setTheme(curTheme() === 'light' ? 'vs' : 'vs-dark'); if (route.cur && route.cur !== 'edita' && route.cur !== 'ide') route({ soft: true }); } // never tear down an open editor
$('#theme').addEventListener('click', () => {
  const next = curTheme() === 'light' ? 'dark' : 'light';
  document.documentElement.dataset.theme = next;
  try { localStorage.setItem('theme', next); } catch (e) { /* private mode */ }
  themeChanged();
});
matchMedia('(prefers-color-scheme: light)').addEventListener('change', themeChanged);
try { const t = localStorage.getItem('theme'); if (t) document.documentElement.dataset.theme = t; } catch (e) { /* ignore */ }
$('#theme-use').setAttribute('href', curTheme() === 'light' ? '#i-sun' : '#i-moon');
$('#theme').setAttribute('aria-pressed', String(curTheme() === 'dark'));
$('.skip').addEventListener('click', (e) => { e.preventDefault(); $('#main').focus(); });
let curHash = location.hash;
addEventListener('hashchange', async () => {
  if (route.guard && !(await route.guard())) { history.replaceState(null, '', curHash || '#/'); return; }
  curHash = location.hash; route();
});
// Sticky offsets from the real header/banner heights (scroll-padding + sticky group headings).
{
  const root = document.documentElement, side = $('.side'), banner = $('#banner');
  const measure = () => {
    root.style.setProperty('--side-h', getComputedStyle(side).position === 'sticky' && matchMedia('(max-width:899px)').matches ? `${side.offsetHeight}px` : '0px');
    root.style.setProperty('--banner-h', `${banner.offsetHeight}px`);
  };
  const ro = new ResizeObserver(measure); ro.observe(side); ro.observe(banner); addEventListener('resize', measure);
}
// Keyboard nav (single-key shortcuts, WCAG 2.1.4: can be turned off in the ? dialog).
// Never bound to an action button (Puja/Envia/Desa/Sincronitza).
const GO = { t: 'tauler', c: 'calendari', f: 'fitxers', n: 'notes', o: 'forums', z: 'organitza' };
const keysOn = () => { try { return localStorage.getItem('keys') !== 'off'; } catch (e) { return true; } };
const KEYS = [['j / k', 'fila següent / anterior'], ['g t', 'Tauler'], ['g c', 'Calendari'], ['g f', 'Fitxers'], ['g n', 'Notes'], ['g o', 'Fòrums'], ['g z', 'Organitza'], ['/', 'filtra el tauler'], ['Esc', 'esborra el filtre']];
function showKeys() {
  const on = h('input', { type: 'checkbox', checked: keysOn(), onchange: (e) => { try { localStorage.setItem('keys', e.target.checked ? 'on' : 'off'); } catch (x) { /* ignore */ } } });
  confirmDialog('Dreceres de teclat', h('div', {},
    h('dl', { class: 'keys' }, KEYS.flatMap(([k, d]) => [h('dt', {}, h('kbd', {}, k)), h('dd', {}, d)])),
    h('label', {}, on, 'Activa les dreceres d\'una tecla')), 'Tanca', true);
}
document.addEventListener('keydown', (e) => {
  const tg = e.target;
  if (e.key === 'Escape' && $('#toast').classList.contains('on') && !tg.closest('dialog')) hideToast();
  if (tg.id === 'filter' && e.key === 'Escape') { tg.value = ''; tg.dispatchEvent(new Event('input')); return; }
  if (e.ctrlKey || e.metaKey || e.altKey || tg.closest('input,textarea,select,[contenteditable],dialog')) return;
  const k = e.key;
  if (k === '?') { showKeys(); return; }
  if (!keysOn()) return;
  if (keyNav.g && GO[k]) { location.hash = `#/${GO[k]}`; keyNav.g = 0; return; }
  keyNav.g = 0;
  if (k === 'g') { keyNav.g = 1; setTimeout(() => (keyNav.g = 0), 1000); }
  else if (k === '/') { const f = $('#filter'); if (f) { e.preventDefault(); f.focus(); } }
  else if (k === 'j' || k === 'k') {
    const rows = [...document.querySelectorAll('#main .row')].filter((r) => r.offsetParent);
    if (!rows.length) return;
    const i = rows.indexOf(document.activeElement);
    rows[Math.min(rows.length - 1, Math.max(0, i + (k === 'j' ? 1 : -1)))].focus();
  }
});
const keyNav = { g: 0 };
$('#keys').addEventListener('click', showKeys);
addEventListener('DOMContentLoaded', () => { loadCourses().catch(() => {}).finally(() => route()); refreshStatus(); setInterval(refreshStatus, 60000); });

const NOISE = new Set(['meta.json', 'index.md', 'last-sync.json']);
views.curs = async (short) => {
  const c = await api(`/api/courses/${enc(short)}`);
  const tasks = (await api('/api/tasks')).filter((tv) => tv.task.course === short && tv.task.kind !== 'post' && tv.task.ai_confirmed !== false);
  const nDone = tasks.filter(isDone).length;
  const tasksCol = tasks.length
    ? h('div', {}, h('h2', {}, 'Tasques'), h('div', { class: 'cards' }, tasks.map((tv) => card(tv, true))))
    : h('div', {}, h('h2', {}, 'Tasques'), h('p', { class: 'dim' }, 'Cap tasca en aquest curs.'));
  // Sections with no visible files are dropped (no empty accordions).
  const secs = c.sections.map((s) => ({ name: s.name, mods: s.modules.map((m) => [m, m.files.filter((f) => !f.is_dir && !NOISE.has(f.name))]).filter(([, fs]) => fs.length) })).filter((s) => s.mods.length);
  const contents = h('div', {}, h('h2', {}, 'Continguts'),
    secs.length ? secs.map((s, i) => h('details', { open: i < 2 },
      h('summary', {}, s.name),
      s.mods.map(([m, fs]) => {
        return fs.length ? [m.name === 'Fitxers' ? null : h('h3', {}, m.name), h('ul', { class: 'list' },
          fs.map((f) => h('li', {}, h('a', { href: `#/fitxers/${enc(f.rel)}` },
            ic('file'), h('span', { class: 'fn' }, f.name), h('span', { class: 'mono dim' }, fsize(f.size))))))] : null;
      }))) : empty('Sense continguts', 'Sincronitza per carregar-los.'));
  return h('section', { class: 'v-curs' },
    h('h1', {}, h('span', { class: 'dot lg', style: { '--h': hue(short) } }), ' ', c.title),
    tasks.length ? h('p', { class: 'sub' }, bar(nDone, tasks.length, `Tasques lliurades de ${c.title}`, '', `${nDone} de ${tasks.length} lliurades`)) : null,
    h('div', { class: 'two' }, tasksCol, contents));
};

views.fitxers = async (...parts) => {
  const rel = parts.join('/');
  const info = await api(`/api/files?path=${enc(rel)}`);
  const segs = rel.split('/').filter(Boolean);
  const crumbs = h('nav', { class: 'crumbs', 'aria-label': 'Ruta' });
  crumbs.append(segs.length ? h('a', { href: '#/fitxers' }, '~/smx') : h('span', { 'aria-current': 'page' }, '~/smx'));
  segs.reduce((acc, p, i) => {
    const r = acc ? `${acc}/${p}` : p;
    crumbs.append(h('span', { class: 'sep', 'aria-hidden': 'true' }, '/'),
      i === segs.length - 1 ? h('span', { 'aria-current': 'page' }, p) : h('a', { href: `#/fitxers/${enc(r)}` }, p));
    return r;
  }, '');
  if (info.kind === 'dir') {
    const es = [...info.entries].sort((a, b) => (b.is_dir - a.is_dir) || a.name.localeCompare(b.name, 'ca'));
    return h('section', { class: 'v-fitxers' }, h('h1', {}, 'Fitxers'), crumbs,
      es.length
        ? h('div', { class: 'tablewrap', role: 'region', 'aria-label': 'Llista de fitxers' },
            h('table', {}, h('caption', { class: 'sr-only' }, `Contingut de ${rel || '~/smx'}`),
              h('thead', {}, h('tr', {}, h('th', { scope: 'col' }, 'Nom'), h('th', { scope: 'col', class: 'num' }, 'Mida'))),
              h('tbody', {}, es.map((e) => h('tr', {},
                h('td', {}, h('a', { class: 'v-fname', href: `#/fitxers/${enc(e.rel)}` },
                  h('span', { class: e.is_dir ? 'v-glyph dir' : 'v-glyph' }, e.is_dir ? ic('folder') : ic('file')),
                  h('span', {}, e.name), e.is_dir ? h('span', { class: 'sr-only' }, ' (carpeta)') : null)),
                h('td', { class: 'num' }, e.is_dir ? '' : fsize(e.size)))))))
        : empty('Carpeta buida', 'No hi ha cap fitxer aquí.'));
  }
  const raw = `/api/raw?path=${enc(rel)}`;
  const name = rel.split('/').pop();
  const body = h('div', {});
  preview(body, { path: rel, name, size: info.size });
  return h('section', { class: 'v-fitxers' }, h('h1', {}, name), crumbs,
    h('div', { class: 'actions' },
      h('a', { class: 'btn', href: raw, download: name }, 'Descarrega'),
      fileExtras.map((f) => f(info)),
      h('span', { class: 'mono dim' }, fsize(info.size))),
    h('div', { class: 'v-preview' }, body));
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
  const wd = new Intl.DateTimeFormat('ca-ES', { weekday: 'long' });
  const long = new Intl.DateTimeFormat('ca-ES', { weekday: 'long', day: 'numeric', month: 'long' });
  const ev = (tv, extra) => h('a', {
    href: `#/tasca/${enc(tv.task.id)}`, title: tv.task.title,
    class: (isDone(tv) ? 'done ' : left(tv.task.due).s === 'late' ? 'late ' : '') + (extra ? 'x' : ''),
  }, isDone(tv) ? ic('check') : left(tv.task.due).s === 'late' ? ic('warn') : h('span', { class: 'dot', style: { '--h': hue(tv.task.course) } }),
    h('span', { class: 'et' }, isDone(tv) ? h('span', { class: 'sr-only' }, 'Lliurada: ') : left(tv.task.due).s === 'late' ? h('span', { class: 'sr-only' }, 'Endarrerida: ') : null, tv.task.title));
  const cells = [];
  for (let i = 0; i < 42; i++) {
    const d = new Date(start); d.setDate(start.getDate() + i);
    const evs = byDay[key(d)] || [];
    const today = key(d) === key(now);
    const cls = 'd' + (d.getMonth() !== m - 1 ? ' out' : '') + (today ? ' today' : '');
    const cell = h('div', { class: cls, ...(evs.length ? { role: 'group', 'aria-label': long.format(d) } : {}) },
      h('strong', today ? { 'aria-current': 'date' } : {}, d.getDate()),
      h('span', { class: 'wd' }, wd.format(d)),
      evs.slice(0, 3).map((tv) => ev(tv)),
      evs.slice(3).map((tv) => ev(tv, true)));
    if (evs.length > 3) {
      const n = evs.length - 3;
      const more = h('button', { type: 'button', class: 'ghost more', 'aria-expanded': 'false', 'aria-label': `+${n} tasques més`,
        onclick: () => more.setAttribute('aria-expanded', cell.classList.toggle('open')) }, `+${n}`);
      cell.append(more);
    }
    cells.push(cell);
  }
  const nav = (dm) => { const d = new Date(y, m - 1 + dm, 1); return `#/calendari/${d.getFullYear()}-${d.getMonth() + 1}`; };
  const isNow = y === now.getFullYear() && m === now.getMonth() + 1;
  const title0 = new Intl.DateTimeFormat('ca-ES', { month: 'long', year: 'numeric' }).format(first);
  const title = title0.charAt(0).toUpperCase() + title0.slice(1);
  return h('section', { class: 'v-cal' },
    h('div', { class: 'cal-head' },
      h('h1', {}, title),
      h('div', { class: 'seg', role: 'group', 'aria-label': 'Navegació del calendari' },
        h('a', { class: 'btn', id: 'cal-prev', href: nav(-1), 'aria-label': 'Mes anterior' }, ic('arrow-l')),
        h('a', { class: 'btn', id: 'cal-today', href: '#/calendari', ...(isNow ? { 'aria-current': 'true' } : {}) }, 'Avui'),
        h('a', { class: 'btn', id: 'cal-next', href: nav(1), 'aria-label': 'Mes següent' }, ic('arrow-r')))),
    h('div', { class: 'cal' },
      ['dl', 'dt', 'dc', 'dj', 'dv', 'ds', 'dg'].map((d) => h('div', { class: 'dim', 'aria-hidden': 'true' }, d)),
      cells));
};

views.notes = async () => {
  const gs = await api('/api/grades');
  if (!gs.length) return h('section', { class: 'v-notes' }, h('h1', {}, 'Notes'), empty('Encara no hi ha notes', 'Sincronitza per carregar-les.'));
  const pct = (s) => { const n = parseFloat(String(s ?? '').replace(',', '.')); return Number.isFinite(n) ? Math.max(0, Math.min(100, n)) : null; };
  const nm = (g) => cname(g.course) || `Curs ${g.course_id}`;
  const shown = (g) => (g.items || []).filter((i) => i.itemname);
  const dash = (v) => (!v || /^\s*[-–—]\s*$/.test(String(v)) ? '–' : v);
  const marked = (g) => shown(g).some((i) => dash(i.gradeformatted) !== '–' || dash(i.percentageformatted) !== '–');
  const blank = gs.filter((g) => !marked(g));
  return h('section', { class: 'v-notes' }, h('h1', {}, 'Notes'), gs.filter(marked).map((g) => {
    const name = nm(g), items = shown(g);
    const ps = items.map((i) => pct(i.percentageformatted)).filter((n) => n != null);
    const avg = ps.length ? Math.round(ps.reduce((a, b) => a + b, 0) / ps.length) : null;
    return h('div', { class: 'course-notes' },
      h('h2', {}, h('span', { class: 'dot', style: { '--h': hue(g.course || String(g.course_id)) } }), name,
        avg != null ? h('span', { class: 'badge', title: 'Mitjana dels percentatges' }, `Mitjana ${avg}%`) : null),
      h('div', { class: 'tablewrap', role: 'region', 'aria-label': `Notes de ${name}` },
        h('table', {}, h('caption', { class: 'sr-only' }, `Notes de ${name}`),
          h('thead', {}, h('tr', {}, h('th', { scope: 'col' }, 'Element'), h('th', { scope: 'col', class: 'num' }, 'Nota'), h('th', { scope: 'col', class: 'num' }, '%'))),
          h('tbody', {}, items.map((i) => {
            const p = pct(i.percentageformatted);
            return h('tr', {}, h('td', {}, i.itemname), h('td', { class: 'num' }, dash(i.gradeformatted)),
              h('td', { class: 'num pc' }, p != null ? h('span', { class: 'bar acc', 'aria-hidden': 'true' }, h('i', { style: { width: `${p}%` } })) : null, dash(i.percentageformatted)));
          })))));
  }), blank.length ? h('p', { class: 'dim' }, `Sense notes: ${blank.map(nm).join(', ')}`) : null);
};

// Organitza: last organize log + organize/undo/delivered/Apple actions. Text only via h().
views.organitza = async () => {
  const log = await api('/api/organize');
  const KINDS = { move: 'Mou', replace: 'Substitueix', module: 'Mòdul' };
  const act = (label, run, confirm) => h('button', { type: 'button', class: confirm ? '' : 'primary', onclick: async (e) => {
    const b = e.currentTarget;
    if (b.getAttribute('aria-busy') === 'true') return;
    if (confirm && !(await confirmDialog(confirm[0], h('p', {}, confirm[1]), label))) { b.focus(); return; }
    b.setAttribute('aria-busy', 'true'); b.setAttribute('aria-disabled', 'true');
    try { const r = await api(run[0], { method: 'POST' }); toast(run[1](r), !!run[2]?.(r)); await route({ soft: true }); }
    catch (err) { toast(err.message, true); }
    b.removeAttribute('aria-busy'); b.removeAttribute('aria-disabled');
  } }, label);
  const errs = (r) => (r.errors?.length ? r.errors.join('; ') : null);
  const withErr = (msg) => (r) => msg(r) + (errs(r) ? ` · ${errs(r)}` : '');
  const ops = log.ops || [];
  return h('section', { class: 'v-organitza' }, h('h1', {}, 'Organitza'),
    h('div', { class: 'actions' },
      act('Ordena ~/smx', ['/api/organize', withErr((r) => `Ordenat: ${r.ops} canvis${r.skipped.length ? `, ${r.skipped.length} omesos` : ''}`), errs],
        ['Ordenar ~/smx?', 'Mourà fitxers dins de ~/smx. Es pot desfer amb «Desfés».']),
      act('Desfés', ['/api/organize/undo', (r) => `Desfet: ${r.restored} restaurats${r.skipped.length ? `, ${r.skipped.length} no s'han pogut desfer` : ''}`],
        ['Desfer l\'última ordenació?', 'Es restauraran els fitxers a la ubicació anterior.']),
      act('Sincronitza lliurades', ['/api/delivered-sync', withErr((r) => `Lliurades: ${r.tasks} tasques, ${r.downloaded} baixats`), errs]),
      act('Sincronitza amb Apple', ['/api/apple-sync', withErr((r) => `Apple: ${r.created} noves, ${r.updated} actualitzades, ${r.done} fetes, ${r.deleted} esborrades`), errs])),
    h('h2', {}, 'Última ordenació'),
    !ops.length ? empty('Encara no s\'ha ordenat res') : h('div', {},
      h('p', { class: 'dim' }, `Execució ${log.run} · ${when(log.at)} · ${ops.length} operacions`),
      h('ul', { class: 'oplog' }, ops.map((o) => h('li', {}, h('b', {}, KINDS[o.kind] || o.kind), ' ', h('span', { class: 'mono' }, o.from), ' → ', h('span', { class: 'mono' }, o.to))))));
};

views.forums = async () => {
  const posts = (await api('/api/tasks')).filter((tv) => tv.task.kind === 'post').sort((a, b) => b.task.opens - a.task.opens);
  const item = (tv) => {
    const isRead = tv.local.status === 'llegit';
    return h('details', { class: 'fpost' + (isRead ? ' read' : ''), ontoggle: async (e) => {
      const d = e.target;
      if (!d.open || d.dataset.loaded) return;
      d.dataset.loaded = '1';
      try {
        const full = await api(`/api/tasks/${enc(tv.task.id)}`);
        d.append(sandboxed(full.task.body_html, `Avís: ${tv.task.title}`));
        if (tv.local.status !== 'llegit') {
          await api(`/api/tasks/${enc(tv.task.id)}/status`, { method: 'POST', body: { status: 'llegit' } });
          tv.local.status = 'llegit';
          d.classList.add('read');
          d.querySelector('.ud')?.remove();
          forumPill();
        }
      } catch (err) { delete d.dataset.loaded; toast(err.message, true); }
    } }, h('summary', {},
      isRead ? null : h('span', { class: 'ud' }, h('span', { class: 'sr-only' }, 'Sense llegir')),
      h('span', { class: 'ft' }, tv.task.title),
      h('span', { class: 'dim mono fm' }, `${cname(tv.task.course)} · ${when(tv.task.opens)}`)));
  };
  const unread = posts.filter((p) => p.local.status !== 'llegit');
  const read = posts.filter((p) => p.local.status === 'llegit');
  const grp = (name, n) => h('h2', { class: 'grp' }, name, h('span', { class: 'n' }, n));
  return h('section', { class: 'v-forums' }, h('h1', {}, 'Fòrums'),
    unread.length ? [grp('Sense llegir', unread.length), h('div', { class: 'cards' }, unread.map(item))] : empty('Tot llegit', posts.length ? 'No tens cap avís nou.' : 'No hi ha avisos.'),
    read.length ? [grp('Llegits', read.length), h('div', { class: 'cards' }, read.map(item))] : null);
};

const kb = (n) => `${Math.max(1, Math.ceil(n / 1024))} KB`;
taskExtras.push((tv) => {
  if (tv.task.kind !== 'assign') return null;
  const id = enc(tv.task.id), files = tv.local.files, submitted = tv.task.moodle_status === 'submitted';
  const after = async (r, btn) => { toast(r.warning || `Moodle: ${MSTATUS[r.status] || r.status}`); await route({ soft: true }); $(btn)?.focus(); };
  const noUp = submitted || !files.length, noSend = tv.task.moodle_status !== 'draft';
  const whyUp = submitted ? 'Ja enviada.' : 'Vincula primer un fitxer per poder pujar-lo.';
  const whySend = submitted ? 'Ja enviada.' : 'Envia es desbloqueja quan hi ha un esborrany a Moodle.';
  const up = h('button', { type: 'button', id: 'puja', 'aria-disabled': String(noUp), 'aria-describedby': 'puja-hint', onclick: async () => {
    if (noUp) { toast(whyUp); return; }
    const sizes = await Promise.all(files.map((f) => api(`/api/files?path=${enc(f)}`).then((i) => kb(i.size), () => 'no trobat')));
    const body = h('div', {}, h('p', {}, `Tasca: ${tv.task.title} (${tv.task.course})`),
      h('p', {}, 'Es pujaran aquests fitxers com a esborrany. Substitueixen els que ja hi hagi a Moodle. Si la tasca no admet esborranys, quedarà enviada directament.'),
      h('ul', {}, files.map((f, i) => h('li', {}, `${f} (${sizes[i]})`))));
    if (!(await confirmDialog('Puja com a esborrany?', body, 'Puja'))) { up.focus(); return; }
    try { await after(await api(`/api/tasks/${id}/upload`, { method: 'POST', body: { files } }), '#puja'); }
    catch (e) { toast(e.message, true); }
  } }, 'Puja (esborrany)');
  const send = h('button', { type: 'button', id: 'envia', 'aria-disabled': String(noSend), 'aria-describedby': 'envia-hint', onclick: async () => {
    if (noSend) { toast(whySend); return; }
    const body = h('p', {}, `Enviaràs "${tv.task.title}" per qualificar. Després ja no la podràs modificar.`);
    if (!(await confirmDialog('Envia per qualificar?', body, 'Envia'))) { send.focus(); return; }
    try { await after(await api(`/api/tasks/${id}/submit`, { method: 'POST' }), '#envia'); }
    catch (e) { toast(e.message, true); }
  } }, 'Envia per qualificar');
  return [up, h('p', { class: 'dim hint', id: 'puja-hint' }, noUp ? whyUp : ''), send, h('p', { class: 'dim hint', id: 'envia-hint' }, noSend ? whySend : '')];
});

const OFFICE = /\.(docx?|odt|rtf|xlsx?|ods|csv|pptx?|odp)$/i;
const editButton = (rel) => h('a', { class: 'btn primary', href: `#/edita/${enc(rel)}` }, 'Edita');
fileExtras.push((info) => (OFFICE.test(info.rel) ? editButton(info.rel) : null));
taskExtras.push((tv) => { const f = tv.local.files.find((x) => OFFICE.test(x)); return f ? editButton(f) : null; });

views.edita = async (...parts) => {
  const rel = parts.join('/');
  const openLocal = h('button', { type: 'button', onclick: async () => { try { await api('/api/open', { method: 'POST', body: { path: rel } }); } catch (e) { toast(e.message, true); } } }, "Obre amb l'aplicació de l'ordinador");
  const fallback = (msg) => h('div', { class: 'v-edita-fb' }, h('h1', { class: 'sr-only' }, 'Edita'), errorPanel(msg), h('div', { class: 'actions' }, openLocal));
  let cfg;
  try { cfg = await api('/api/office/config', { method: 'POST', body: { path: rel } }); }
  catch (e) { return fallback(e.status === 503 ? 'OnlyOffice no disponible.' : e.message); }
  // DocsAPI replaces #oo with an iframe sized 100% of its parent, so the parent carries the height.
  const holder = h('div', { class: 'v-oo' }, h('div', { id: 'oo' }));
  const bar = h('div', { class: 'v-editbar' },
    h('a', { class: 'btn ghost', href: `#/fitxers/${enc(cfg.rel)}` }, ic('arrow-l'), 'Torna al fitxer'),
    h('span', { class: 'v-editname', title: cfg.rel }, cfg.rel.split('/').pop()));
  const wrap = h('section', { class: 'v-edita' }, h('h1', { class: 'sr-only' }, `Edita ${cfg.rel}`), bar, holder);
  // DocsAPI injects an untitled iframe: name it once it appears.
  new MutationObserver((_, o) => { const f = holder.querySelector('iframe'); if (f) { f.title = 'Editor de documents'; o.disconnect(); } }).observe(holder, { childList: true, subtree: true });
  // api.js does getElementById(placeholderId): it takes the id only, so build once the holder is in the document.
  const open = () => { if (holder.isConnected) new window.DocsAPI.DocEditor('oo', cfg.config); };
  wrap.addEventListener('mounted', () => {
    if (window.DocsAPI) return open();
    const s = h('script', { src: cfg.api_js });
    s.onload = open;
    s.onerror = () => wrap.replaceChildren(fallback('OnlyOffice no disponible.'));
    document.head.append(s);
  }, { once: true });
  return wrap;
};

// IDE: Monaco (cdnjs AMD loader, loaded on first visit) + sandboxed live preview for html/svg/md.
const IDE_FILE = /\.(txt|md|html?|svg|css|js|json|xml|csv|java|py|sql|sh|php|ts|c|cpp|h|rs|go|ya?ml)$/i;
fileExtras.push((info) => (IDE_FILE.test(info.rel) ? h('a', { class: 'btn', href: `#/ide/${enc(info.rel)}` }, "Obre a l'IDE") : null));
// ide-helpers-start
const LANG = { js: 'javascript', ts: 'typescript', py: 'python', md: 'markdown', htm: 'html', sh: 'shell', yml: 'yaml', rs: 'rust', txt: 'plaintext', csv: 'plaintext', cpp: 'cpp', h: 'c' };
function langOf(rel) { const e = (rel.split('.').pop() || '').toLowerCase(); return LANG[e] || e; }
function previewDoc(lang, text) {
  if (lang === 'html') return text;
  if (lang === 'svg' || lang === 'xml' && /<svg[\s>]/.test(text)) return `<body style="margin:0">${text}</body>`;
  if (lang === 'markdown') return `<body style="font:16px/1.6 system-ui;max-width:70ch;margin:16px auto;padding:0 16px">${window.marked.parse(text)}</body>`;
  return null;
}
// ide-helpers-end
const MONACO = 'https://cdnjs.cloudflare.com/ajax/libs/monaco-editor/0.52.2/min', MARKED = 'https://cdnjs.cloudflare.com/ajax/libs/marked/12.0.2/marked.min.js';
const loadJs = (src) => (loadJs[src] ??= new Promise((ok, ko) => { const s = h('script', { src }); s.onload = ok; s.onerror = () => { delete loadJs[src]; ko(new Error('No s\'ha pogut carregar ' + src)); }; document.head.append(s); }));
async function loadMonaco() {
  if (window.monaco) return;
  window.MonacoEnvironment = { getWorkerUrl: () => 'data:text/javascript;charset=utf-8,' + enc(`self.MonacoEnvironment={baseUrl:'${MONACO}/'};importScripts('${MONACO}/vs/base/worker/workerMain.js');`) };
  await loadJs(`${MONACO}/vs/loader.min.js`);
  window.require.config({ paths: { vs: `${MONACO}/vs` } });
  await new Promise((ok, ko) => window.require(['vs/editor/editor.main'], ok, ko));
}

views.ide = async (...parts) => {
  const rel = parts.join('/');
  let f;
  try { f = await api(`/api/ide/file?path=${enc(rel)}`); }
  catch (e) { return e.status === 415 ? empty('No és un fitxer de text', 'Aquest fitxer no es pot editar com a text.', h('a', { class: 'btn', href: `#/fitxers/${enc(rel)}` }, 'Torna al fitxer')) : errorPanel(e.message); }
  let sha = f.sha, dirty = false, ed;
  const lang = langOf(rel), live = ['html', 'svg', 'markdown'].includes(lang) || lang === 'xml';
  const state = h('span', { class: 'v-dirty dim', role: 'status' });
  const setDirty = (d) => { dirty = d; setText(state, d ? 'Canvis sense desar' : ''); };
  const post = (url, body) => api(url, { method: 'POST', body });
  const save = async () => {
    const content = ed.getValue();
    try { sha = (await post('/api/save', { path: rel, content, base_sha: sha })).sha; setDirty(false); toast('Desat'); return; }
    catch (e) {
      const k = e.body?.error;
      if (k === 'teacher_original') {
        if (!(await confirmDialog('Crea còpia _amin?', h('p', {}, "Aquest fitxer és l'original del professor i no es pot modificar. Es crearà una còpia _amin amb els teus canvis."), 'Crea còpia'))) return;
        try {
          const c = await post('/api/ide/copy', { path: rel });
          await post('/api/save', { path: c.rel, content, base_sha: c.sha });
          setDirty(false); location.hash = `#/ide/${enc(c.rel)}`;
        } catch (e2) { toast(e2.message, true); }
      } else if (k === 'conflict') {
        if (!(await confirmDialog('El fitxer ha canviat al disc', h('p', {}, 'Algú ha modificat aquest fitxer des que el vas obrir. Vols sobreescriure\'l amb el teu text?'), 'Sobreescriu'))) return;
        try { sha = (await post('/api/save', { path: rel, content, base_sha: e.body.sha })).sha; setDirty(false); toast('Desat'); } catch (e2) { toast(e2.message, true); }
      } else toast(e.message, true);
    }
  };
  const frame = live ? h('iframe', { class: 'v-idepv', sandbox: '', title: 'Previsualització en directe' }) : null;
  let t;
  const refresh = () => { clearTimeout(t); t = setTimeout(() => { frame.srcdoc = previewDoc(lang === 'xml' ? 'svg' : lang, ed.getValue()) ?? ''; }, 250); };
  const box = h('div', { class: 'v-idebox', role: 'group', 'aria-label': 'Editor de codi' });
  const bar = h('div', { class: 'v-editbar' },
    h('a', { class: 'btn ghost', href: `#/fitxers/${enc(rel)}` }, ic('arrow-l'), 'Torna al fitxer'),
    h('span', { class: 'v-editname', title: rel }, rel.split('/').pop()), state,
    h('button', { type: 'button', class: 'primary', onclick: save }, 'Desa'));
  const wrap = h('section', { class: 'v-ide' }, h('h1', { class: 'sr-only' }, `IDE ${rel}`), bar,
    f.teacher && h('p', { class: 'v-idenote dim' }, 'Original del professor: en desar es crearà una còpia _amin.'),
    h('div', { class: 'v-idesplit' + (live ? ' live' : '') }, box, frame));
  wrap.addEventListener('mounted', async () => {
    try {
      await Promise.all([loadMonaco(), lang === 'markdown' && loadJs(MARKED)]);
      if (!wrap.isConnected) return;
      ed = window.monaco.editor.create(box, { value: f.content, language: lang, theme: curTheme() === 'light' ? 'vs' : 'vs-dark', automaticLayout: true, ariaLabel: 'Editor de codi' });
      ed.addCommand(window.monaco.KeyMod.CtrlCmd | window.monaco.KeyCode.KeyS, save);
      ed.onDidChangeModelContent(() => { setDirty(true); if (live) refresh(); });
      if (live) refresh();
      const stop = (ev) => { if (dirty) ev.preventDefault(); };
      addEventListener('beforeunload', stop);
      route.guard = async () => {
        if (dirty && !(await confirmDialog('Canvis sense desar', h('p', {}, 'Si surts es perdran els canvis.'), 'Descarta'))) return false;
        ed.dispose(); removeEventListener('beforeunload', stop); route.guard = null; return true;
      };
    } catch (e) { wrap.replaceChildren(errorPanel(e.message)); }
  }, { once: true });
  return wrap;
};
