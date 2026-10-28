'use strict';
// Cabina — interfaz. El audio lo produce el motor nativo (Rust); aquí solo se dibuja y se envían órdenes.

const T = window.__TAURI__;
const invoke = (cmd, args) => T.core.invoke(cmd, args);
const listen = (ev, cb) => T.event.listen(ev, cb);

const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];
const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
const mod = (a, n) => ((a % n) + n) % n;
const dpr = () => Math.min(2, window.devicePixelRatio || 1);
const fmt = s => { if (!isFinite(s)) return '--:--.-'; s = Math.max(0, s); const m = Math.floor(s / 60), r = s - m * 60; return `${String(m).padStart(2, '0')}:${r.toFixed(1).padStart(4, '0')}`; };
const fmtDur = s => s > 0 ? `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, '0')}` : '—';
const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));

const HC = ['#2fd17a', '#ff4d6d', '#ffb020', '#a874ff', '#22d3ee', '#f472b6', '#e5e7eb', '#84cc16'];
const WAVE_COLORS = [['#2a7fe6', '#8fcbff', '#ffffff'], ['#e8641f', '#ffbd8e', '#ffffff']];
const BASE_PPS = 110;
const ZOOMS = [0.5, 0.75, 1, 1.5, 2, 3, 4];
const AUDIO_EXT = ['mp3', 'wav', 'wave', 'aif', 'aiff', 'flac', 'm4a', 'mp4', 'aac', 'ogg', 'alac'];
let zoomIdx = 2;
let quantize = true;

function toast(msg, err = false) {
  const t = document.createElement('div');
  t.className = 'toast' + (err ? ' err' : '');
  t.textContent = msg;
  $('#toasts').appendChild(t);
  setTimeout(() => t.remove(), err ? 6000 : 3500);
}
const fail = e => toast(String(e?.message ?? e), true);

const store = {
  get(k, d) { try { const v = localStorage.getItem('cabina.' + k); return v == null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem('cabina.' + k, JSON.stringify(v)); } catch {} },
};

/* ======================================================================
   Colección
   ====================================================================== */
const ROW_H = 30;
const lib = { tracks: [], byId: new Map(), view: [], sort: store.get('sort', { key: 'added', dir: -1 }), q: '', sel: new Set(), anchor: null };
const COLS = [
  { key: 'idx', label: '#' }, { key: 'title', label: 'Título' }, { key: 'artist', label: 'Artista' },
  { key: 'bpm', label: 'BPM' }, { key: 'key', label: 'Tono' }, { key: 'duration', label: 'Tiempo' },
  { key: 'style', label: 'Estilo' }, { key: 'lufs', label: 'LUFS' }, { key: null, label: 'Cargar' },
];

function camelotRel(a, b) {
  if (!a || !b) return '';
  if (a === b) return 'match';
  const pa = /^(\d+)([AB])$/.exec(a), pb = /^(\d+)([AB])$/.exec(b);
  if (!pa || !pb) return '';
  const na = +pa[1], nb = +pb[1];
  if (pa[2] === pb[2] && (mod(na - nb, 12) === 1 || mod(nb - na, 12) === 1)) return 'near';
  if (na === nb) return 'near';
  return '';
}

function refKey() {
  const st = engine.state;
  if (!st) return null;
  const playing = decks.find(d => st.decks[d.i].playing && d.track);
  const d = playing || decks.find(d => d.track);
  return d?.track?.key || null;
}

function sortValue(t, key) {
  switch (key) {
    case 'key': { const m = /^(\d+)([AB])$/.exec(t.key || ''); return m ? +m[1] * 2 + (m[2] === 'B' ? 1 : 0) : 99; }
    case 'lufs': return t.lufs ?? -99;
    case 'title': case 'artist': case 'genre': return (t[key] || '').toLocaleLowerCase('es');
    case 'style': return (t.style || '~').toLocaleLowerCase('es');
    default: return t[key] ?? 0;
  }
}

function refreshView() {
  const q = lib.q.trim().toLocaleLowerCase('es');
  let v = lib.tracks;
  if (q) v = v.filter(t => `${t.title} ${t.artist} ${t.album} ${t.genre} ${t.style || ''} ${t.key || ''} ${t.keyName || ''}`.toLocaleLowerCase('es').includes(q));
  const { key, dir } = lib.sort;
  if (key && key !== 'idx') v = [...v].sort((a, b) => { const x = sortValue(a, key), y = sortValue(b, key); return (x < y ? -1 : x > y ? 1 : 0) * dir; });
  else v = [...v].sort((a, b) => ((a.added - b.added) || a.path.localeCompare(b.path)) * (dir || 1));
  lib.view = v;
  $('#spacer').style.height = `${v.length * ROW_H}px`;
  $('#empty').hidden = lib.tracks.length > 0;
  $('#libCount').textContent = q ? `${v.length} de ${lib.tracks.length} pistas` : `${lib.tracks.length} ${lib.tracks.length === 1 ? 'pista' : 'pistas'}`;
  const pending = lib.tracks.filter(t => t.status === 'pending').length;
  $('#libProgress').textContent = pending ? `Analizando ${pending} ${pending === 1 ? 'pista' : 'pistas'}…` : '';
  renderHead();
  renderRows();
}

function renderHead() {
  $('#thead').innerHTML = COLS.map(c => c.key
    ? `<button data-sort="${c.key}" class="${lib.sort.key === c.key ? 'sorted' + (lib.sort.dir > 0 ? ' asc' : '') : ''}">${c.label}</button>`
    : `<span class="lbl" style="text-align:right">${c.label}</span>`).join('');
}
$('#thead').addEventListener('click', e => {
  const b = e.target.closest('[data-sort]'); if (!b) return;
  const key = b.dataset.sort;
  lib.sort = lib.sort.key === key ? { key, dir: -lib.sort.dir } : { key, dir: key === 'added' || key === 'idx' ? -1 : 1 };
  store.set('sort', lib.sort);
  refreshView();
});

const STYLES = { list: [], unclassified: 'Sin clasificar' };
function styleCell(t) {
  if (t.status !== 'ready' || !STYLES.list.length) return `<span class="dim">${esc(t.style || t.genre || '—')}</span>`;
  const cls = !t.style ? 'none' : (t.styleConfidence ?? 1) < 0.5 ? 'unsure' : '';
  const title = t.style ? `${t.style}${t.styleConfidence != null && t.styleConfidence < 1 ? ` · seguridad ${Math.round(t.styleConfidence * 100)} %` : ''}${t.genre ? ` · etiqueta: ${t.genre}` : ''}` : (t.genre ? `Sin estilo · etiqueta: ${t.genre}` : 'Sin estilo');
  const opts = [...STYLES.list, STYLES.unclassified].map(s => `<option${s === t.style ? ' selected' : ''}>${esc(s)}</option>`).join('');
  return `<select class="stylesel ${cls}" data-style-for="${t.id}" title="${esc(title)}" aria-label="Estilo">${t.style ? '' : `<option value="" selected>${esc(t.genre ? t.genre + ' ·' : '—')}</option>`}${opts}</select>`;
}

let rowsRaf = 0;
function renderRows() {
  cancelAnimationFrame(rowsRaf);
  rowsRaf = requestAnimationFrame(() => {
    const body = $('#tbody');
    const first = Math.max(0, Math.floor(body.scrollTop / ROW_H) - 6);
    const count = Math.ceil(body.clientHeight / ROW_H) + 12;
    const rk = refKey();
    const loaded = decks.map(d => d.trackId);
    let html = '';
    for (let i = first; i < Math.min(lib.view.length, first + count); i++) {
      const t = lib.view[i];
      const cls = ['row', lib.sel.has(t.id) ? 'sel' : '', t.status, loaded[0] === t.id ? 'loaded-0' : '', loaded[1] === t.id ? 'loaded-1' : ''].join(' ');
      let last;
      if (t.status === 'ready') last = `<div class="loadbtns"><button data-load="0" style="--lc:var(--deck-1)" aria-label="Cargar en el plato 1">1</button><button data-load="1" style="--lc:var(--deck-2)" aria-label="Cargar en el plato 2">2</button></div>`;
      else if (t.status === 'pending') last = '<span class="pill">Analizando…</span>';
      else if (t.status === 'missing') last = '<span class="pill err" title="El archivo se movió o se borró">No encontrado</span>';
      else last = `<span class="pill err" title="${esc(t.error)}">Error</span>`;
      const rel = rk && t.key ? camelotRel(rk, t.key) : '';
      html += `<div class="${cls}" data-id="${t.id}" data-i="${i}">
        <div class="num dim">${i + 1}</div><div class="title" title="${esc(t.title)}">${esc(t.title)}</div><div class="dim">${esc(t.artist)}</div>
        <div class="num">${t.bpm ? t.bpm.toFixed(2) : '—'}</div><div>${t.key ? `<span class="key ${rel}" title="${esc(t.keyName)}">${t.key}</span>` : '<span class="dim">—</span>'}</div>
        <div class="num">${fmtDur(t.duration)}</div><div>${styleCell(t)}</div><div class="num dim">${t.lufs != null ? t.lufs.toFixed(1) : '—'}</div><div>${last}</div></div>`;
    }
    const rows = $('#rows');
    rows.style.transform = `translateY(${first * ROW_H}px)`;
    rows.innerHTML = html;
  });
}
$('#tbody').addEventListener('scroll', renderRows, { passive: true });
new ResizeObserver(renderRows).observe($('#tbody'));
$('#search').addEventListener('input', e => { lib.q = e.target.value; refreshView(); });

function upsertTrack(t) {
  const i = lib.tracks.findIndex(x => x.id === t.id);
  if (i >= 0) lib.tracks[i] = t; else lib.tracks.push(t);
  lib.byId.set(t.id, t);
  for (const d of decks) if (d.trackId === t.id) { d.track = t; deckMeta(d); }
  scheduleRefresh();
  if (!org.el.hidden) schedulePaintOrganize();
}
let refreshTimer = 0, orgTimer = 0;
function schedulePaintOrganize() { if (!orgTimer) orgTimer = setTimeout(() => { orgTimer = 0; paintOrganize(); }, 150); }
function scheduleRefresh() { if (!refreshTimer) refreshTimer = setTimeout(() => { refreshTimer = 0; refreshView(); }, 120); }

async function loadLibrary() {
  lib.tracks = await invoke('library_list');
  lib.byId = new Map(lib.tracks.map(t => [t.id, t]));
  refreshView();
}

function updateSelButtons() {
  $('#removeBtn').hidden = $('#reanalyzeBtn').hidden = lib.sel.size === 0;
}

// Cambio manual de estilo
$('#rows').addEventListener('change', async e => {
  const sel = e.target.closest('[data-style-for]'); if (!sel) return;
  try { upsertTrack(await invoke('set_track_style', { id: sel.dataset.styleFor, style: sel.value || null })); } catch (err) { fail(err); }
});

// Selección, carga y arrastre de filas
$('#rows').addEventListener('click', e => {
  const lb = e.target.closest('[data-load]');
  const row = e.target.closest('.row'); if (!row || e.target.closest('select')) return;
  if (lb) { loadToDeck(+lb.dataset.load, row.dataset.id); return; }
  const id = row.dataset.id, idx = +row.dataset.i;
  if (e.shiftKey && lib.anchor != null) {
    const [a, b] = [Math.min(lib.anchor, idx), Math.max(lib.anchor, idx)];
    if (!e.metaKey) lib.sel.clear();
    for (let k = a; k <= b; k++) lib.sel.add(lib.view[k].id);
  } else if (e.metaKey || e.ctrlKey) {
    lib.sel.has(id) ? lib.sel.delete(id) : lib.sel.add(id);
    lib.anchor = idx;
  } else {
    lib.sel.clear(); lib.sel.add(id); lib.anchor = idx;
  }
  updateSelButtons(); renderRows();
});
$('#rows').addEventListener('dblclick', e => {
  const row = e.target.closest('.row'); if (!row || e.target.closest('[data-load]')) return;
  const st = engine.state;
  const free = decks.find(d => !st?.decks[d.i].playing && !d.trackId) || decks.find(d => !st?.decks[d.i].playing);
  if (free) loadToDeck(free.i, row.dataset.id); else toast('Los dos platos están sonando', true);
});

let rowDrag = null;
$('#rows').addEventListener('pointerdown', e => {
  const row = e.target.closest('.row');
  if (!row || e.button !== 0 || e.target.closest('button, select')) return;
  rowDrag = { id: row.dataset.id, x: e.clientX, y: e.clientY, active: false };
});
window.addEventListener('pointermove', e => {
  if (!rowDrag) return;
  if (!rowDrag.active && Math.hypot(e.clientX - rowDrag.x, e.clientY - rowDrag.y) > 6) {
    const t = lib.byId.get(rowDrag.id);
    if (!t || t.status !== 'ready') { rowDrag = null; return; }
    rowDrag.active = true;
    $('#ghost').textContent = `${t.title}${t.artist ? ' · ' + t.artist : ''}`;
    $('#ghost').hidden = false;
  }
  if (rowDrag.active) {
    const g = $('#ghost'); g.style.left = `${e.clientX + 12}px`; g.style.top = `${e.clientY + 8}px`;
    const over = document.elementFromPoint(e.clientX, e.clientY)?.closest('[data-deck]');
    decks.forEach(d => d.el.classList.toggle('drop', !!over && +over.dataset.deck === d.i));
  }
});
window.addEventListener('pointerup', e => {
  if (!rowDrag) return;
  if (rowDrag.active) {
    const over = document.elementFromPoint(e.clientX, e.clientY)?.closest('[data-deck]');
    if (over) loadToDeck(+over.dataset.deck, rowDrag.id);
    $('#ghost').hidden = true;
    decks.forEach(d => d.el.classList.remove('drop'));
  }
  rowDrag = null;
});

async function removeSelected() {
  const ids = [...lib.sel].filter(id => !decks.some(d => d.trackId === id));
  if (!ids.length) { toast('No se pueden quitar pistas cargadas en un plato', true); return; }
  await invoke('library_remove', { ids });
  lib.tracks = lib.tracks.filter(t => !ids.includes(t.id));
  ids.forEach(id => lib.byId.delete(id));
  lib.sel.clear(); updateSelButtons(); refreshView();
  toast(`${ids.length} ${ids.length === 1 ? 'pista quitada' : 'pistas quitadas'} de la colección (los archivos no se borran)`);
}
$('#removeBtn').addEventListener('click', removeSelected);
$('#reanalyzeBtn').addEventListener('click', async () => {
  const ids = [...lib.sel];
  await invoke('library_reanalyze', { ids });
  ids.forEach(id => { const t = lib.byId.get(id); if (t) t.status = 'pending'; });
  refreshView();
});

async function addPaths(paths, deckIdx = null) {
  if (!paths?.length) return;
  const added = await invoke('library_add', { paths });
  added.forEach(upsertTrack);
  if (!added.length) toast('No hay pistas nuevas: ya estaban en la colección o no son archivos de audio');
  else toast(`${added.length} ${added.length === 1 ? 'pista añadida' : 'pistas añadidas'}. Analizando…`);
  if (deckIdx != null && added.length) pendingLoad.set(added[0].id, deckIdx);
}
const pendingLoad = new Map();

async function pick(opts) {
  if (T.dialog?.open) return T.dialog.open(opts);
  return invoke('plugin:dialog|open', { options: opts });
}
$('#addFolderBtn').addEventListener('click', async () => {
  try {
    const r = await pick({ directory: true, multiple: true, title: 'Elige las carpetas de tu música' });
    if (r) addPaths(Array.isArray(r) ? r : [r]);
  } catch (e) { fail(e); }
});
$('#addFilesBtn').addEventListener('click', async () => {
  try {
    const r = await pick({ multiple: true, title: 'Elige archivos de audio', filters: [{ name: 'Audio', extensions: AUDIO_EXT }] });
    if (r) addPaths(Array.isArray(r) ? r : [r]);
  } catch (e) { fail(e); }
});

/* ======================================================================
   Platos
   ====================================================================== */
const deckHTML = i => `
<article class="deck panel" data-deck="${i}" style="--c:var(--deck-${i + 1})">
  <div class="deck-head">
    <div class="deck-num">${i + 1}</div>
    <div style="min-width:0"><div class="t-title">Sin pista</div><div class="t-meta"><span class="key"></span><span class="t-artist">Arrastra una pista desde la colección</span></div></div>
    <div class="deck-bpm"><div class="bpm"><span class="bpmv">—</span><small>BPM</small></div><div class="pct">±0.00%</div></div>
  </div>
  <div class="deck-time"><span class="el">00:00.0</span><span class="bars">—</span><span class="rem">-00:00.0</span></div>
  <canvas class="ov" aria-label="Vista general de la pista del plato ${i + 1}"></canvas>
  <div class="ctl">
    <div class="transport">
      <button class="round cue" aria-label="Cue">CUE</button>
      <button class="round play" aria-label="Reproducir o pausar">
        <svg class="i-play" viewBox="0 0 24 24"><path d="M7 4.5v15L19.5 12z"/></svg>
        <svg class="i-pause" viewBox="0 0 24 24"><path d="M6 5h4.2v14H6zM13.8 5H18v14h-4.2z"/></svg>
      </button>
    </div>
    <div class="pads">${'ABCDEFGH'.split('').map((l, k) => `<button class="pad" data-i="${k}" style="--hc:var(--hc-${l.toLowerCase()})" title="Hot cue ${l}: clic para fijar o saltar · Mayús+clic o clic derecho para borrar">${l}</button>`).join('')}</div>
  </div>
  <div class="ctl">
    <div class="loops"><span class="lbl">Loop</span>
      <button class="btn" data-lr="0.5" title="Mitad">½</button>
      ${[1, 2, 4, 8, 16].map(n => `<button class="btn" data-beats="${n}" title="Loop de ${n} ${n === 1 ? 'beat' : 'beats'}">${n}</button>`).join('')}
      <button class="btn" data-lr="2" title="Doble">×2</button>
      <span class="sep"></span>
      <button class="btn" data-jump="-4" title="Saltar 4 beats atrás">◀◀</button><button class="btn" data-jump="4" title="Saltar 4 beats adelante">▶▶</button>
    </div>
  </div>
  <div class="tempo">
    <button class="btn small mt" title="Master Tempo (keylock): cambia el tempo sin cambiar el tono">MT</button>
    <span class="lbl">Tempo</span>
    <input id="tempo${i}" type="range" min="-8" max="8" step="0.02" value="0" aria-label="Tempo del plato ${i + 1}">
    <button class="btn small treset" title="Volver a 0 %">0%</button>
    <select id="trange${i}" aria-label="Rango del tempo"><option value="6">±6%</option><option value="8" selected>±8%</option><option value="16">±16%</option><option value="50">±50%</option></select>
    <button class="btn small sync" title="Iguala el tempo y la fase con el otro plato">Sync</button>
  </div>
  <div class="gridrow">
    <span class="lbl">Rejilla</span>
    <button class="btn small" data-grid="half" title="Dividir el BPM entre 2">×½</button>
    <button class="btn small" data-grid="double" title="Multiplicar el BPM por 2">×2</button>
    <button class="btn small" data-grid="-0.005" title="Mover la rejilla 5 ms antes">◂</button>
    <button class="btn small" data-grid="0.005" title="Mover la rejilla 5 ms después">▸</button>
    <button class="btn small" data-grid="here" title="Poner un beat (inicio de compás) en la posición actual">Compás aquí</button>
  </div>
</article>`;

const knob = (param, label, opts = {}) =>
  `<div class="kw"><div class="knob" role="slider" tabindex="0" aria-label="${label}" data-param="${param}" data-min="${opts.min ?? -1}" data-max="${opts.max ?? 1}" data-def="${opts.def ?? 0}" ${opts.bipolar === false ? '' : 'data-bipolar'} style="${opts.c ? `--c:${opts.c}` : ''}"><i></i></div><span class="lbl">${label}</span></div>`;

const mixerHTML = `
<aside class="mixer panel" aria-label="Mezclador">
  <div class="strips">${[0, 1].map(i => `
    <div class="strip" style="--c:var(--deck-${i + 1})">
      <div class="ch-num">${i + 1}</div>
      <div class="strip-body">
        <div class="knobs">
          ${knob('trim' + i, 'Trim')}
          ${knob('hi' + i, 'Hi')}${knob('mid' + i, 'Mid')}${knob('low' + i, 'Low')}
          ${knob('filter' + i, 'Filtro')}
        </div>
        <div class="fader-col"><div class="meter"><i></i></div><div id="fader${i}" class="vfader" role="slider" tabindex="0" aria-orientation="vertical" aria-label="Fader del canal ${i + 1}" data-param="fader${i}"><div class="vf-track"></div><div class="vf-cap"></div></div></div>
      </div>
      <button class="cuebtn" data-cue="${i}" aria-pressed="false" title="Escuchar el canal ${i + 1} en los auriculares">
        <svg viewBox="0 0 24 24"><path d="M12 3a9 9 0 0 0-9 9v6a3 3 0 0 0 3 3h2v-8H5v-1a7 7 0 0 1 14 0v1h-3v8h2a3 3 0 0 0 3-3v-6a9 9 0 0 0-9-9z"/></svg>CUE</button>
    </div>`).join('')}
  </div>
  <div class="xf"><span>1</span><input id="xfader" type="range" min="0" max="1" step="0.005" value="0.5" aria-label="Crossfader" title="Doble clic: centrar"><span>2</span></div>
  <div class="mbottom">
    ${knob('master', 'Master', { min: 0, max: 1, def: 0.8, bipolar: false })}
    ${knob('cue_mix', 'Cue/Mix', { min: 0, max: 1, def: 0.3, bipolar: false })}
    ${knob('hp_level', 'Auric.', { min: 0, max: 1, def: 0.7, bipolar: false })}
  </div>
</aside>`;

$('#booth').innerHTML = deckHTML(0) + mixerHTML + deckHTML(1);

const decks = [0, 1].map(i => ({
  i, el: $(`.deck[data-deck="${i}"]`), zc: $(`#zoom${i}`), oc: $(`.deck[data-deck="${i}"] .ov`),
  trackId: null, requestedId: null, track: null, wave: null, ovKey: '', ovImg: null,
  cueGen: -1, loadGen: -1, lvl: [0, 0], shown: {}, colors: WAVE_COLORS[i], drag: null, bendTimer: 0,
}));

const send = (i, action, value) => invoke('deck', { deck: i, action, value: value ?? null }).catch(fail);

function loadToDeck(i, id) {
  const st = engine.state?.decks[i];
  if (st?.playing) { toast(`El plato ${i + 1} está sonando. Páralo antes de cargar otra pista.`, true); return; }
  const t = lib.byId.get(id);
  if (!t || t.status !== 'ready') { toast('La pista aún no está lista', true); return; }
  decks[i].requestedId = id;
  invoke('load_track', { deck: i, id }).catch(fail);
}

function deckMeta(d) {
  const t = d.track;
  $('.t-title', d.el).textContent = t ? t.title : 'Sin pista';
  $('.t-artist', d.el).textContent = t ? (t.artist || '—') : 'Arrastra una pista desde la colección';
  $('.key', d.el).textContent = t?.key || '';
  $('.key', d.el).title = t?.keyName || '';
}

async function fetchWave(d, id) {
  try {
    const buf = await invoke('track_waveform', { id });
    const u8 = new Uint8Array(buf);
    const dv = new DataView(u8.buffer, u8.byteOffset);
    if (String.fromCharCode(...u8.slice(0, 4)) !== 'CBW1') throw new Error('formato');
    const rate = dv.getUint32(4, true), n = dv.getUint32(8, true);
    const lo = u8.subarray(12, 12 + n), mi = u8.subarray(12 + n, 12 + 2 * n), hi = u8.subarray(12 + 2 * n, 12 + 3 * n);
    if (d.trackId === id) { d.wave = { rate, n, lo, mi, hi }; d.ovKey = ''; }
  } catch { d.wave = null; }
}

for (const d of decks) {
  const el = d.el;
  $('.play', el).addEventListener('click', () => send(d.i, 'toggle'));
  const cue = $('.cue', el);
  cue.addEventListener('pointerdown', e => { cue.setPointerCapture(e.pointerId); send(d.i, 'cue_down'); });
  cue.addEventListener('pointerup', () => send(d.i, 'cue_up'));
  $$('.pad', el).forEach(p => {
    p.addEventListener('click', e => send(d.i, e.shiftKey ? 'hot_clear' : 'hot', +p.dataset.i));
    p.addEventListener('contextmenu', e => { e.preventDefault(); send(d.i, 'hot_clear', +p.dataset.i); });
  });
  $$('[data-beats]', el).forEach(b => b.addEventListener('click', () => send(d.i, 'loop', +b.dataset.beats)));
  $$('[data-lr]', el).forEach(b => b.addEventListener('click', () => send(d.i, b.dataset.lr === '2' ? 'loop_double' : 'loop_half')));
  $$('[data-jump]', el).forEach(b => b.addEventListener('click', () => send(d.i, 'jump', +b.dataset.jump)));
  $('.sync', el).addEventListener('click', () => send(d.i, 'sync', engine.state?.decks[d.i].synced ? 0 : 1));
  $('.mt', el).addEventListener('click', () => {
    const on = !engine.state?.decks[d.i].keylock;
    send(d.i, 'keylock', on ? 1 : 0);
    store.set('keylock' + d.i, on);
  });
  const tr = $(`#tempo${d.i}`);
  tr.addEventListener('input', () => send(d.i, 'tempo', 1 + tr.value / 100));
  tr.addEventListener('dblclick', () => send(d.i, 'tempo', 1));
  $('.treset', el).addEventListener('click', () => send(d.i, 'tempo', 1));
  const range = $(`#trange${d.i}`);
  range.value = String(store.get('range' + d.i, 8));
  const applyRange = () => { tr.min = -range.value; tr.max = range.value; store.set('range' + d.i, +range.value); };
  range.addEventListener('change', applyRange); applyRange();
  $$('[data-grid]', el).forEach(b => b.addEventListener('click', () => editGrid(d, b.dataset.grid)));

  // Vista general: clic o arrastre para saltar
  const ovSeek = e => { const st = engine.state?.decks[d.i]; if (!st?.duration) return; const r = d.oc.getBoundingClientRect(); send(d.i, 'seek', clamp((e.clientX - r.left) / r.width, 0, 1) * st.duration); };
  d.oc.addEventListener('pointerdown', e => { d.oc.setPointerCapture(e.pointerId); d.ovDrag = true; ovSeek(e); });
  d.oc.addEventListener('pointermove', e => { if (d.ovDrag) ovSeek(e); });
  d.oc.addEventListener('pointerup', () => { d.ovDrag = false; });

  // Onda ampliada: parado = desplazarse; sonando = nudge (acelerar o frenar)
  d.zc.addEventListener('pointerdown', e => { if (!d.trackId) return; d.zc.setPointerCapture(e.pointerId); d.drag = { x: e.clientX }; });
  d.zc.addEventListener('pointermove', e => {
    if (!d.drag) return;
    const dx = e.clientX - d.drag.x; d.drag.x = e.clientX;
    const st = engine.state?.decks[d.i];
    if (st?.playing) {
      send(d.i, 'bend', clamp(1 - dx * 0.02, 0.7, 1.3));
      clearTimeout(d.bendTimer);
      d.bendTimer = setTimeout(() => send(d.i, 'bend', 1), 60);
    } else {
      send(d.i, 'scrub', -dx / (BASE_PPS * ZOOMS[zoomIdx]));
    }
  });
  const endDrag = () => { if (d.drag) { d.drag = null; send(d.i, 'bend', 1); } };
  d.zc.addEventListener('pointerup', endDrag); d.zc.addEventListener('pointercancel', endDrag);
}

async function editGrid(d, op) {
  const st = engine.state?.decks[d.i];
  if (!d.trackId || !st) return;
  let bpm = st.bpm, grid = st.grid;
  const beat = 60 / bpm;
  if (op === 'half') bpm /= 2;
  else if (op === 'double') bpm *= 2;
  else if (op === 'here') grid = mod(st.pos, beat * 4);
  else grid += +op;
  grid = mod(grid, 60 / bpm);
  try {
    const t = await invoke('track_set_grid', { id: d.trackId, bpm, grid });
    upsertTrack(t);
  } catch (e) { fail(e); }
}

/* ======================================================================
   Mezclador
   ====================================================================== */
const PARAM_DEFAULTS = {
  trim0: 0, trim1: 0, hi0: 0, mid0: 0, low0: 0, hi1: 0, mid1: 0, low1: 0, filter0: 0, filter1: 0,
  fader0: 0.85, fader1: 0.85, cue0: 0, cue1: 0, xfader: 0.5, xf_curve: 0, master: 0.8, cue_mix: 0.3, hp_level: 0.7,
  limiter: 1, quantize: 1, routing: 1, auto_gain: 1,
};
const params = { ...PARAM_DEFAULTS, ...store.get('params', {}) };
// el filtro y el trim no se recuerdan entre sesiones: arrancar siempre neutro es más seguro
['filter0', 'filter1', 'xfader'].forEach(k => { params[k] = PARAM_DEFAULTS[k]; });
let saveTimer = 0;
function setParam(name, value) {
  params[name] = value;
  invoke('set_param', { name, value }).catch(fail);
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => store.set('params', params), 400);
}

function paintKnob(k) {
  const min = +k.dataset.min, max = +k.dataset.max, v = params[k.dataset.param];
  const norm = (v - min) / (max - min);
  k.style.setProperty('--v', norm.toFixed(4));
  k.style.setProperty('--from', 'bipolar' in k.dataset ? '0.5' : '0');
  k.setAttribute('aria-valuenow', v.toFixed(2));
  k.classList.toggle('kill', /^(hi|mid|low)\d$/.test(k.dataset.param) && v <= -0.999);
}
for (const k of $$('.knob')) {
  const min = +k.dataset.min, max = +k.dataset.max, def = +k.dataset.def, name = k.dataset.param;
  k.setAttribute('aria-valuemin', min); k.setAttribute('aria-valuemax', max);
  const set = v => { setParam(name, clamp(v, min, max)); paintKnob(k); };
  let drag = null;
  k.addEventListener('pointerdown', e => { k.setPointerCapture(e.pointerId); drag = { y: e.clientY, v: params[name] }; });
  k.addEventListener('pointermove', e => {
    if (!drag) return;
    const sens = (e.shiftKey ? 0.0015 : 0.006) * (max - min);
    let v = drag.v + (drag.y - e.clientY) * sens;
    // pequeño "imán" en el centro de los mandos bipolares
    if ('bipolar' in k.dataset && Math.abs(v) < 0.03 * (max - min)) v = 0;
    set(v);
  });
  k.addEventListener('pointerup', () => { drag = null; });
  k.addEventListener('dblclick', () => set(def));
  k.addEventListener('wheel', e => { e.preventDefault(); set(params[name] - Math.sign(e.deltaY) * 0.03 * (max - min)); }, { passive: false });
  k.addEventListener('keydown', e => {
    const step = (e.shiftKey ? 0.01 : 0.05) * (max - min);
    if (e.key === 'ArrowUp' || e.key === 'ArrowRight') { set(params[name] + step); e.preventDefault(); }
    if (e.key === 'ArrowDown' || e.key === 'ArrowLeft') { set(params[name] - step); e.preventDefault(); }
    if (e.key === 'Home' || e.key === '0') set(def);
  });
  paintKnob(k);
}
// Faders verticales propios (iguales en cualquier versión de macOS)
function paintFader(f) {
  const v = params[f.dataset.param];
  f.style.setProperty('--v', v.toFixed(4));
  f.setAttribute('aria-valuenow', v.toFixed(2));
}
for (const f of $$('.vfader')) {
  const name = f.dataset.param;
  f.setAttribute('aria-valuemin', 0); f.setAttribute('aria-valuemax', 1);
  const set = v => { setParam(name, clamp(v, 0, 1)); paintFader(f); };
  const fromY = e => { const r = f.getBoundingClientRect(); return 1 - (e.clientY - r.top - 8) / (r.height - 16); };
  let drag = null;
  f.addEventListener('pointerdown', e => {
    f.setPointerCapture(e.pointerId);
    const cap = $('.vf-cap', f).getBoundingClientRect();
    // si se agarra la tapa se arrastra sin saltos; si se pulsa el carril, salta ahí
    drag = e.clientY >= cap.top && e.clientY <= cap.bottom ? { y: e.clientY, v: params[name] } : null;
    if (!drag) { set(fromY(e)); drag = { y: e.clientY, v: params[name] }; }
  });
  f.addEventListener('pointermove', e => { if (drag) set(drag.v + (drag.y - e.clientY) / (f.getBoundingClientRect().height - 16)); });
  f.addEventListener('pointerup', () => { drag = null; });
  f.addEventListener('wheel', e => { e.preventDefault(); set(params[name] - Math.sign(e.deltaY) * 0.03); }, { passive: false });
  f.addEventListener('keydown', e => {
    const step = e.shiftKey ? 0.01 : 0.05;
    if (e.key === 'ArrowUp' || e.key === 'ArrowRight') { set(params[name] + step); e.preventDefault(); }
    if (e.key === 'ArrowDown' || e.key === 'ArrowLeft') { set(params[name] - step); e.preventDefault(); }
  });
  paintFader(f);
}
for (const i of [0, 1]) {
  const cb = $(`[data-cue="${i}"]`);
  const paintCue = () => { cb.classList.toggle('on', params['cue' + i] > 0.5); cb.setAttribute('aria-pressed', params['cue' + i] > 0.5); };
  cb.addEventListener('click', () => { setParam('cue' + i, params['cue' + i] > 0.5 ? 0 : 1); paintCue(); });
  paintCue();
}
const xf = $('#xfader');
xf.value = params.xfader;
xf.addEventListener('input', () => setParam('xfader', +xf.value));
xf.addEventListener('dblclick', () => { xf.value = 0.5; setParam('xfader', 0.5); });
const qBtn = $('#qBtn');
const paintQ = () => { quantize = params.quantize > 0.5; qBtn.classList.toggle('on', quantize); qBtn.setAttribute('aria-pressed', quantize); };
qBtn.addEventListener('click', () => { setParam('quantize', quantize ? 0 : 1); paintQ(); });
paintQ();

/* ======================================================================
   Estado del motor y dibujo
   ====================================================================== */
const engine = { state: null, at: 0, inflight: false, gen: null, info: null };

function livePos(st) {
  let p = st.pos + (performance.now() - engine.at) / 1000 * st.speed;
  if (st.loopStart != null && p >= st.loopEnd) p = st.loopStart + mod(p - st.loopStart, st.loopEnd - st.loopStart);
  return Math.min(p, st.duration);
}

function setText(d, sel, v) { if (d.shown[sel] !== v) { d.shown[sel] = v; $(sel, d.el).textContent = v; } }
function setClass(el, cls, on) { if (el.classList.contains(cls) !== on) el.classList.toggle(cls, on); }

function applyState(s) {
  if (engine.gen != null && s.engineGen !== engine.gen) {
    // cambió la frecuencia del dispositivo: el motor se reinició y hay que recargar las pistas
    for (const d of decks) if (d.requestedId) { const id = d.requestedId; setTimeout(() => loadToDeck(d.i, id), 200); }
  }
  engine.gen = s.engineGen;
  for (const d of decks) {
    const st = s.decks[d.i];
    if (st.trackId !== d.trackId) {
      d.trackId = st.trackId;
      d.track = st.trackId ? lib.byId.get(st.trackId) : null;
      d.wave = null; d.ovKey = '';
      deckMeta(d);
      if (st.trackId) fetchWave(d, st.trackId);
      renderRows();
    }
    if (st.loadGen !== d.loadGen) {
      d.loadGen = st.loadGen;
      d.cueGen = st.cueGen;
      if (d.trackId) { send(d.i, 'keylock', store.get('keylock' + d.i, false) ? 1 : 0); }
    } else if (st.cueGen !== d.cueGen) {
      d.cueGen = st.cueGen;
      if (d.trackId) {
        invoke('track_save_cues', { id: d.trackId, cue: st.cue, hot: st.hot }).catch(fail);
        const t = lib.byId.get(d.trackId); if (t) { t.cue = st.cue; t.hot = st.hot; }
      }
    }
    const el = d.el;
    setClass($('.play', el), 'on', st.playing);
    setClass($('.cue', el), 'on', !st.playing && d.trackId != null && Math.abs(st.pos - st.cue) < 0.002);
    setClass($('.sync', el), 'on', st.synced);
    setClass($('.mt', el), 'on', st.keylock);
    setClass($('.mt', el), 'stretch', st.stretching);
    $$('[data-beats]', el).forEach(b => setClass(b, 'on', st.loopStart != null && Math.abs(st.loopBeats - b.dataset.beats) < 1e-6));
    $$('.pad', el).forEach((p, k) => setClass(p, 'set', st.hot[k] != null));
    const tr = $(`#tempo${d.i}`);
    const pct = (st.rate - 1) * 100;
    if (document.activeElement !== tr || st.synced) {
      if (Math.abs(pct) > +tr.max) { const r = $(`#trange${d.i}`); r.value = [6, 8, 16, 50].find(v => v >= Math.abs(pct)) || 50; tr.min = -r.value; tr.max = r.value; }
      if (Math.abs(tr.value - pct) > 0.001) tr.value = pct;
    }
    for (const c of [0, 1]) d.lvl[c] = Math.max(st.peak[c], d.lvl[c] * 0.86);
    const lv = Math.max(d.lvl[0], d.lvl[1]);
    const db = clamp((20 * Math.log10(lv + 1e-6) + 42) / 42, 0, 1);
    $(`.strip:nth-child(${d.i + 1}) .meter i`).style.clipPath = `inset(${(100 - db * 100).toFixed(1)}% 0 0 0)`;
  }
  // master
  engine.mlvl = engine.mlvl || [0, 0];
  ['#mmL', '#mmR'].forEach((sel, c) => {
    engine.mlvl[c] = Math.max(s.master[c], engine.mlvl[c] * 0.86);
    const v = clamp((20 * Math.log10(engine.mlvl[c] + 1e-6) + 42) / 42, 0, 1);
    $(sel).style.clipPath = `inset(0 ${(100 - v * 100).toFixed(1)}% 0 0)`;
  });
  setClass($('#limInd'), 'on', s.limiter < 0.97);
  const ms = (s.buffer / s.sampleRate * 1000).toFixed(1);
  $('#stFormat').textContent = `${(s.sampleRate / 1000).toFixed(1)} kHz · ${s.buffer} muestras (${ms} ms)`;
  $('#stLatency').textContent = s.latencyMs > 0 ? `latencia ${s.latencyMs.toFixed(1)} ms` : '';
  $('#stLoad').textContent = `DSP ${(s.dspLoad * 100).toFixed(0)} %`;
}

function readouts(d, st, p) {
  if (!d.trackId) {
    setText(d, '.el', '00:00.0'); setText(d, '.rem', '-00:00.0'); setText(d, '.bpmv', '—'); setText(d, '.pct', '±0.00%'); setText(d, '.bars', '—');
    return;
  }
  setText(d, '.el', fmt(p));
  const rem = st.duration - p;
  setText(d, '.rem', '-' + fmt(rem));
  setClass($('.rem', d.el), 'ending', st.playing && rem < 30);
  setText(d, '.bpmv', (st.bpm * st.rate).toFixed(2));
  const pct = (st.rate - 1) * 100;
  setText(d, '.pct', `${pct >= 0 ? '+' : '−'}${Math.abs(pct).toFixed(2)}%`);
  const beat = 60 / st.bpm;
  const bi = Math.floor((p - st.grid) / beat + 1e-4);
  setText(d, '.bars', bi < 0 ? '—' : `${Math.floor(bi / 4) + 1}.${mod(bi, 4) + 1}`);
}

function fitCanvas(c) {
  const w = Math.round(c.clientWidth * dpr()), h = Math.round(c.clientHeight * dpr());
  if (c.width !== w || c.height !== h) { c.width = w; c.height = h; }
}

function drawZoom(d, st, p) {
  const c = d.zc; fitCanvas(c);
  const g = c.getContext('2d'), W = c.width, H = c.height, k = dpr();
  g.fillStyle = '#07080b'; g.fillRect(0, 0, W, H);
  if (!d.trackId || !st) {
    g.fillStyle = '#5a6170'; g.font = `${13 * k}px Barlow, sans-serif`; g.textAlign = 'center';
    g.fillText(d.loading ? 'Cargando…' : 'Arrastra una pista aquí', W / 2, H / 2 + 4 * k); return;
  }
  const pps = BASE_PPS * ZOOMS[zoomIdx] * k, cx = W / 2, mid = H / 2;
  const X = s => cx + (s - p) * pps;
  const t0 = p - cx / pps, t1 = p + cx / pps;
  if (st.loopStart != null) { g.fillStyle = 'rgba(255,176,32,.16)'; g.fillRect(X(st.loopStart), 0, (st.loopEnd - st.loopStart) * pps, H); }
  const b = 60 / st.bpm;
  for (let n = Math.ceil((t0 - st.grid) / b); ; n++) {
    const s = st.grid + n * b; if (s > t1) break; if (s < 0) continue;
    const down = mod(n, 4) === 0;
    g.fillStyle = down ? 'rgba(255,255,255,.34)' : 'rgba(255,255,255,.1)';
    g.fillRect(Math.round(X(s)), 0, k, H);
    if (down) { g.fillStyle = mod(n, 64) === 0 ? '#ffb020' : 'rgba(255,77,94,.9)'; g.fillRect(Math.round(X(s)) - 2 * k, 0, 5 * k, 3 * k); }
  }
  const w = d.wave;
  if (w) {
    const amp = mid - 3 * k, step = Math.max(1, Math.round(k));
    const layers = [[0, d.colors[0]], [1, d.colors[1]], [2, d.colors[2]]];
    for (const [layer, col] of layers) {
      g.fillStyle = col; g.beginPath();
      for (let x = 0; x < W; x += step) {
        const i = Math.floor((p + (x - cx) / pps) * w.rate);
        if (i < 0 || i >= w.n) continue;
        const v = layer === 0 ? w.lo[i] + w.mi[i] + w.hi[i] : layer === 1 ? w.mi[i] + w.hi[i] : w.hi[i];
        const h = Math.min(255, v) / 255 * amp; if (h < 0.5) continue;
        g.rect(x, mid - h, step, h * 2);
      }
      g.fill();
    }
  }
  st.hot.forEach((h, n) => {
    if (h == null) return; const x = X(h); if (x < -20 || x > W + 20) return;
    g.fillStyle = HC[n]; g.fillRect(Math.round(x), 0, 2 * k, H); g.fillRect(Math.round(x), 0, 14 * k, 14 * k);
    g.fillStyle = '#0b0d11'; g.font = `700 ${11 * k}px "Barlow Condensed", sans-serif`; g.textAlign = 'center';
    g.fillText('ABCDEFGH'[n], x + 7.5 * k, 11 * k);
  });
  const cxp = X(st.cue);
  if (cxp > -10 && cxp < W + 10) { g.fillStyle = '#ffb020'; g.beginPath(); g.moveTo(cxp, H - 9 * k); g.lineTo(cxp - 6 * k, H); g.lineTo(cxp + 6 * k, H); g.fill(); }
  g.fillStyle = '#ffffff'; g.fillRect(Math.round(cx) - k, 0, 2 * k, H);
}

function drawOverview(d, st, p) {
  const c = d.oc; fitCanvas(c);
  const g = c.getContext('2d'), W = c.width, H = c.height, k = dpr();
  g.fillStyle = '#07080b'; g.fillRect(0, 0, W, H);
  if (!d.trackId || !st?.duration || !W) return;
  const key = `${d.trackId}:${W}x${H}:${st.bpm}:${st.grid}:${!!d.wave}`;
  if (d.ovKey !== key) {
    const oc = document.createElement('canvas'); oc.width = W; oc.height = H;
    const og = oc.getContext('2d'), w = d.wave, mid = H / 2;
    if (w) {
      const per = w.n / W;
      [[0, d.colors[0]], [1, d.colors[1]], [2, d.colors[2]]].forEach(([layer, col]) => {
        og.fillStyle = col; og.beginPath();
        for (let x = 0; x < W; x++) {
          let m = 0; const a = Math.floor(x * per), e = Math.max(a + 1, Math.floor((x + 1) * per));
          for (let i = a; i < e && i < w.n; i++) { const v = layer === 0 ? w.lo[i] + w.mi[i] + w.hi[i] : layer === 1 ? w.mi[i] + w.hi[i] : w.hi[i]; if (v > m) m = v; }
          const h = Math.min(255, m) / 255 * (mid - 1); if (h > 0.3) og.rect(x, mid - h, 1, h * 2);
        }
        og.fill();
      });
    }
    // marca cada 16 compases (frase)
    const phrase = 64 * 60 / st.bpm;
    og.fillStyle = 'rgba(230,233,239,.45)';
    for (let s = st.grid + phrase; s < st.duration; s += phrase) og.fillRect(Math.round(s / st.duration * W), H - 4 * k, k, 4 * k);
    d.ovImg = oc; d.ovKey = key;
  }
  g.drawImage(d.ovImg, 0, 0);
  const X = s => s / st.duration * W, px = X(p);
  g.fillStyle = 'rgba(7,8,11,.55)'; g.fillRect(0, 0, px, H);
  if (st.playing && st.duration - p < 30) { g.fillStyle = 'rgba(255,77,94,.22)'; g.fillRect(X(st.duration - 30), 0, W, H); }
  if (st.loopStart != null) { g.fillStyle = 'rgba(255,176,32,.35)'; g.fillRect(X(st.loopStart), 0, Math.max(2, X(st.loopEnd) - X(st.loopStart)), H); }
  st.hot.forEach((h, n) => { if (h != null) { g.fillStyle = HC[n]; g.fillRect(Math.round(X(h)), 0, 2 * k, H); } });
  g.fillStyle = '#ffb020'; g.fillRect(Math.round(X(st.cue)), 0, k, 6 * k);
  g.fillStyle = '#ffffff'; g.fillRect(Math.round(px) - k, 0, 2 * k, H);
}

function drawPhase(s, pos) {
  const c = $('#phase'); fitCanvas(c);
  const g = c.getContext('2d'), W = c.width, H = c.height;
  g.fillStyle = '#13161c'; g.fillRect(0, 0, W, H);
  if (!s || !decks[0].trackId || !decks[1].trackId) return;
  const ph = i => mod((pos[i] - s.decks[i].grid) / (60 / s.decks[i].bpm), 1);
  const diff = mod(ph(1) - ph(0) + 0.5, 1) - 0.5;
  const cx = W / 2, half = W * 0.25;
  g.fillStyle = 'rgba(255,255,255,.25)'; g.fillRect(cx - 1, 0, 2, H);
  const x = cx + diff * 2 * half;
  g.fillStyle = Math.abs(diff) < 0.015 ? '#2fd17a' : Math.abs(diff) < 0.06 ? '#ffb020' : '#ff4d5e';
  g.fillRect(Math.min(cx, x), 1, Math.max(3, Math.abs(x - cx)), H - 2);
}

function frame() {
  if (!engine.inflight) {
    engine.inflight = true;
    invoke('engine_state').then(s => { engine.state = s; engine.at = performance.now(); applyState(s); }).catch(() => {}).finally(() => { engine.inflight = false; });
  }
  const s = engine.state;
  const pos = [0, 0];
  for (const d of decks) {
    const st = s?.decks[d.i];
    const p = st ? livePos(st) : 0;
    pos[d.i] = p;
    drawZoom(d, st, p); drawOverview(d, st, p);
    if (st) readouts(d, st, p);
  }
  drawPhase(s, pos);
  requestAnimationFrame(frame);
}

$('#zIn').addEventListener('click', () => { zoomIdx = Math.min(ZOOMS.length - 1, zoomIdx + 1); });
$('#zOut').addEventListener('click', () => { zoomIdx = Math.max(0, zoomIdx - 1); });

/* ======================================================================
   Ajustes y estado del dispositivo
   ====================================================================== */
async function refreshAudioInfo() {
  try {
    const info = await invoke('audio_info');
    engine.info = info;
    const st = info.status;
    const dev = $('#stDevice');
    dev.textContent = st.error ? 'Sin salida de audio · reintentando' : `${st.device}${st.channels >= 4 ? ` · ${st.channels} canales` : ''}`;
    dev.title = st.error || '';
    dev.className = st.error ? 'err' : '';
  } catch {}
}

function fillSettings() {
  const info = engine.info, s = engine.state;
  if (!info) return;
  const dev = $('#setDevice');
  dev.innerHTML = `<option value="">Predeterminado del sistema</option>` + info.devices.map(n => `<option value="${esc(n)}">${esc(n)}</option>`).join('');
  dev.value = info.selected || '';
  const sr = s?.sampleRate || info.status.sampleRate || 44100;
  const buf = $('#setBuffer');
  buf.innerHTML = info.buffers.map(b => `<option value="${b}">${b} muestras · ${(b / sr * 1000).toFixed(1)} ms</option>`).join('');
  buf.value = String(info.status.buffer || 256);
  $('#setInfo').textContent = info.status.error
    ? info.status.error
    : `${info.status.device}: ${(info.status.sampleRate / 1000).toFixed(1)} kHz, ${info.status.channels} canales, 32 bits float`;
  $('#setRouting').value = String(params.routing);
  $('#setXfCurve').value = String(params.xf_curve);
  $('#setAutoGain').checked = params.auto_gain > 0.5;
  $('#setLimiter').checked = params.limiter > 0.5;
}
const settingsEl = $('#settings');
const closeSettings = () => { settingsEl.hidden = true; $('#settingsBtn').focus(); };
$('#settingsBtn').addEventListener('click', async () => { await refreshAudioInfo(); fillSettings(); settingsEl.hidden = false; $('#setDevice').focus(); });
$('#settingsClose').addEventListener('click', closeSettings);
settingsEl.addEventListener('pointerdown', e => { if (e.target === settingsEl) closeSettings(); });
document.addEventListener('keydown', e => { if (e.key === 'Escape' && !settingsEl.hidden) closeSettings(); });
const reconfigure = () => {
  invoke('audio_configure', { device: $('#setDevice').value || null, buffer: +$('#setBuffer').value }).catch(fail);
  setTimeout(async () => { await refreshAudioInfo(); fillSettings(); }, 700);
};
$('#setDevice').addEventListener('change', reconfigure);
$('#setBuffer').addEventListener('change', reconfigure);
$('#setRouting').addEventListener('change', e => setParam('routing', +e.target.value));
$('#setXfCurve').addEventListener('change', e => setParam('xf_curve', +e.target.value));
$('#setAutoGain').addEventListener('change', e => setParam('auto_gain', e.target.checked ? 1 : 0));
$('#setLimiter').addEventListener('change', e => setParam('limiter', e.target.checked ? 1 : 0));

/* ======================================================================
   Teclado y arrastre desde el Finder
   ====================================================================== */
const HOT_KEYS = { '1': [0, 0], '2': [0, 1], '3': [0, 2], '4': [0, 3], '7': [1, 0], '8': [1, 1], '9': [1, 2], '0': [1, 3] };
const held = new Set();
document.addEventListener('keydown', e => {
  if (e.target.matches('input[type=search], select') || e.metaKey || e.ctrlKey || !settingsEl.hidden || !$('#organize').hidden || e.target.matches('input, select')) return;
  const k = e.key.toLowerCase();
  if (e.repeat && (k === 'x' || k === 'n')) return;
  if (k === 'z') send(0, 'toggle');
  else if (k === 'm') send(1, 'toggle');
  else if (k === 'x' || k === 'n') { held.add(k); send(k === 'x' ? 0 : 1, 'cue_down'); }
  else if (e.code.startsWith('Digit') && HOT_KEYS[e.code.slice(5)]) { const [d, i] = HOT_KEYS[e.code.slice(5)]; send(d, e.shiftKey ? 'hot_clear' : 'hot', i); }
  else if ((e.key === 'Delete' || e.key === 'Backspace') && lib.sel.size && document.activeElement?.closest('#lib')) removeSelected();
  else return;
  e.preventDefault();
});
document.addEventListener('keyup', e => {
  const k = e.key.toLowerCase();
  if (held.has(k)) { held.delete(k); send(k === 'x' ? 0 : 1, 'cue_up'); }
});

listen('tauri://drag-enter', () => { $('#dropHint').hidden = false; });
listen('tauri://drag-leave', () => { $('#dropHint').hidden = true; });
listen('tauri://drag-drop', e => {
  $('#dropHint').hidden = true;
  const { paths, position } = e.payload || {};
  let deckIdx = null;
  if (position) {
    const el = document.elementFromPoint(position.x / (window.devicePixelRatio || 1), position.y / (window.devicePixelRatio || 1));
    const dk = el?.closest('[data-deck]');
    if (dk) deckIdx = +dk.dataset.deck;
  }
  addPaths(paths, deckIdx).catch(fail);
});

listen('track-updated', e => {
  const t = e.payload;
  upsertTrack(t);
  if (t.status === 'ready' && pendingLoad.has(t.id)) { const d = pendingLoad.get(t.id); pendingLoad.delete(t.id); loadToDeck(d, t.id); }
  if (t.status === 'error') toast(`No se pudo analizar «${t.title}»: ${t.error}`, true);
});
listen('deck-loading', e => { const d = decks[e.payload.deck]; d.loading = true; $('.t-title', d.el).textContent = 'Cargando…'; });
listen('deck-loaded', e => { const d = decks[e.payload.deck]; d.loading = false; });
listen('deck-error', e => { const d = decks[e.payload.deck]; d.loading = false; deckMeta(d); toast(e.payload.message, true); });

/* ======================================================================
   Organizar por estilo
   ====================================================================== */
const org = { el: $('#organize'), dest: null, hasKey: false, busy: false };
function setProgress(el, done, total, label) {
  el.hidden = false;
  $('i', el).style.width = `${total ? (done / total) * 100 : 0}%`;
  $('span', el).textContent = label;
}
function paintOrganize() {
  $('#keyBox').hidden = org.hasKey;
  $('#keyOk').hidden = !org.hasKey;
  const ready = lib.tracks.filter(t => t.status === 'ready');
  const pending = ready.filter(t => !t.style).length;
  $('#classifyNew').textContent = pending ? `Clasificar ${pending} ${pending === 1 ? 'pista' : 'pistas'} sin estilo` : 'Todas las pistas tienen estilo';
  $('#classifyNew').disabled = !org.hasKey || !pending || org.busy;
  $('#classifyAll').disabled = !org.hasKey || !ready.length || org.busy;
  $('#runOrganize').disabled = !org.dest || !ready.length || org.busy;
  const counts = new Map();
  for (const t of ready) if (t.style) counts.set(t.style, (counts.get(t.style) || 0) + 1);
  $('#styleChips').innerHTML = [...counts].sort((a, b) => b[1] - a[1])
    .map(([s, n]) => `<button class="chip" type="button" data-chip="${esc(s)}" title="Ver estas pistas en la colección">${esc(s)}<b>${n}</b></button>`).join('');
}
async function refreshStylesInfo() {
  try {
    const info = await invoke('styles_info');
    STYLES.list = info.styles; STYLES.unclassified = info.unclassified;
    org.hasKey = info.hasKey; org.busy = info.busy;
  } catch {}
}
$('#organizeBtn').addEventListener('click', async () => { await refreshStylesInfo(); paintOrganize(); org.el.hidden = false; });
const closeOrganize = () => { org.el.hidden = true; $('#organizeBtn').focus(); };
$('#organizeClose').addEventListener('click', closeOrganize);
org.el.addEventListener('pointerdown', e => { if (e.target === org.el) closeOrganize(); });
document.addEventListener('keydown', e => { if (e.key === 'Escape' && !org.el.hidden) closeOrganize(); });
$('#saveKey').addEventListener('click', async () => {
  try {
    await invoke('set_api_key', { key: $('#apiKey').value });
    $('#apiKey').value = '';
    await refreshStylesInfo(); paintOrganize();
    if (org.hasKey) toast('Clave de API guardada');
  } catch (e) { fail(e); }
});
$('#apiKey').addEventListener('keydown', e => { if (e.key === 'Enter') $('#saveKey').click(); });
$('#changeKey').addEventListener('click', () => { org.hasKey = false; paintOrganize(); $('#apiKey').focus(); });
async function classify(ids) {
  try {
    const n = await invoke('classify_styles', { ids });
    if (!n) { toast('No hay pistas que clasificar'); return; }
    org.busy = true; paintOrganize();
    setProgress($('#clsProgress'), 0, n, `Clasificando 0 de ${n}…`);
  } catch (e) { fail(e); }
}
$('#classifyNew').addEventListener('click', () => classify(null));
$('#classifyAll').addEventListener('click', () => classify(lib.tracks.filter(t => t.status === 'ready').map(t => t.id)));
$('#styleChips').addEventListener('click', e => {
  const c = e.target.closest('[data-chip]'); if (!c) return;
  $('#search').value = lib.q = c.dataset.chip;
  refreshView(); closeOrganize();
});
$('#pickDest').addEventListener('click', async () => {
  try {
    const r = await pick({ directory: true, multiple: false, title: 'Elige dónde crear las carpetas por estilo' });
    if (r) { org.dest = Array.isArray(r) ? r[0] : r; $('#destPath').textContent = org.dest; $('#destPath').title = org.dest; paintOrganize(); }
  } catch (e) { fail(e); }
});
$('#runOrganize').addEventListener('click', async () => {
  const moveFiles = $('#modeMove').checked;
  try {
    const n = await invoke('organize_folders', { dest: org.dest, moveFiles, ids: null });
    org.busy = true; paintOrganize();
    $('#orgResult').hidden = true;
    setProgress($('#orgProgress'), 0, n, `${moveFiles ? 'Moviendo' : 'Copiando'} 0 de ${n}…`);
  } catch (e) { fail(e); }
});
listen('styles-progress', e => {
  const { done, total } = e.payload;
  setProgress($('#clsProgress'), done, total, `Clasificando ${done} de ${total}…`);
});
let lastStyleErr = 0;
listen('styles-error', e => { if (Date.now() - lastStyleErr > 5000) { lastStyleErr = Date.now(); toast(`Un lote falló: ${e.payload}`, true); } });
listen('styles-done', e => {
  const { total, failed, error } = e.payload;
  org.busy = false;
  const ok = total - failed;
  setProgress($('#clsProgress'), total, total, error ? `Detenido: ${error}` : failed ? `Listo: ${ok} clasificadas, ${failed} sin respuesta (vuelve a pulsar para reintentarlas)` : `Listo: ${total} pistas clasificadas`);
  if (error) fail(error);
  paintOrganize(); refreshView();
});
listen('organize-progress', e => {
  const { done, total, style } = e.payload;
  setProgress($('#orgProgress'), done, total, done < total ? `${done} de ${total} · ${style}` : `${total} de ${total}`);
});
listen('organize-done', e => {
  const r = e.payload;
  org.busy = false; paintOrganize();
  const n = r.copied + r.moved;
  const res = $('#orgResult');
  res.hidden = false;
  res.classList.toggle('err', !n);
  res.textContent = `Hecho: ${n} ${n === 1 ? 'pista' : 'pistas'} ${r.moved ? 'movidas' : 'copiadas'} en ${r.folders.length} ${r.folders.length === 1 ? 'carpeta' : 'carpetas'} dentro de ${r.dest}.`
    + (r.missing ? ` ${r.missing} no se encontraron en el disco.` : '')
    + (r.errors.length ? ` ${r.errors.length} errores: ${r.errors.slice(0, 3).join('; ')}` : '');
});
listen('library-reload', () => loadLibrary().catch(fail));

/* ======================================================================
   Arranque
   ====================================================================== */
(async () => {
  for (const [name, value] of Object.entries(params)) invoke('set_param', { name, value }).catch(() => {});
  paintQ();
  try { $('#version').textContent = `Cabina ${await invoke('app_version')}`; } catch {}
  await refreshAudioInfo();
  setInterval(refreshAudioInfo, 3000);
  await refreshStylesInfo();
  await loadLibrary().catch(fail);
  requestAnimationFrame(frame);
})();
