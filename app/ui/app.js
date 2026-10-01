// Archive desktop app: the window's interface.
import { call, on, isApp, onDragDrop } from './api.js';

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

function h(tag, attrs = {}, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v == null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'text') el.textContent = v;
    else if (k === 'html') el.innerHTML = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k === 'style') el.style.cssText = v;
    else if (v === true) el.setAttribute(k, '');
    else el.setAttribute(k, v);
  }
  for (const kid of kids.flat()) if (kid != null && kid !== false) el.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  return el;
}

const svg = (d, size = 16) => {
  const s = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  s.setAttribute('viewBox', '0 0 24 24');
  s.setAttribute('width', size);
  s.setAttribute('height', size);
  s.setAttribute('fill', 'none');
  s.setAttribute('stroke', 'currentColor');
  s.setAttribute('stroke-width', '1.8');
  s.setAttribute('stroke-linecap', 'round');
  s.setAttribute('stroke-linejoin', 'round');
  s.innerHTML = d;
  return s;
};
// Icons from Lucide (https://lucide.dev), ISC License, Copyright (c) Lucide Icons and Contributors.
const BARREL = '<path d="M10 3a41 41 0 000 18"/><path d="M14 3a41 41 0 010 18"/><path d="M16.997 21a2 2 0 001.68-.92 15.25 15.25 0 000-16.16 2 2 0 00-1.68-.92h-10a2 2 0 00-1.681.92 15.25 15.25 0 000 16.16 2 2 0 001.681.92z"/><path d="M3.54 16h16.914"/><path d="M3.54 8h16.914"/>';
let maskIds = 0;
const I = {
  folder: () => svg('<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>'),
  file: () => svg('<path d="M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2z"/><path d="M14 2v5a1 1 0 0 0 1 1h5"/>'),
  link: () => svg('<path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/>'),
  back: () => svg('<path d="m15 18-6-6 6-6"/>'),
  up: () => svg('<path d="m5 12 7-7 7 7"/><path d="M12 19V5"/>'),
  refresh: () => svg('<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/>'),
  newFolder: () => svg('<path d="M12 10v6"/><path d="M9 13h6"/><path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>'),
  trash: () => svg('<path d="M10 11v6"/><path d="M14 11v6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M3 6h18"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>'),
  gear: () => svg('<path d="M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915"/><circle cx="12" cy="12" r="3"/>', 18),
  swap: () => svg('<path d="M8 3 4 7l4 4"/><path d="M4 7h16"/><path d="m16 21 4-4-4-4"/><path d="M20 17H4"/>'),
  right: () => svg('<path d="M5 12h14"/><path d="m12 5 7 7-7 7"/>', 18),
  left: () => svg('<path d="m12 19-7-7 7-7"/><path d="M19 12H5"/>', 18),
  x: () => svg('<path d="M18 6 6 18"/><path d="m6 6 12 12"/>', 14),
  compass: () => svg('<path d="m16.24 7.76-1.804 5.411a2 2 0 0 1-1.265 1.265L7.76 16.24l1.804-5.411a2 2 0 0 1 1.265-1.265z"/><circle cx="12" cy="12" r="10"/>'),
  shield: () => svg('<path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"/><path d="m9 12 2 2 4-4"/>', 18),
  warn: () => svg('<path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3"/><path d="M12 9v4"/><path d="M12 17h.01"/>', 18),
  info: () => svg('<circle cx="12" cy="12" r="10"/><path d="M12 16v-4"/><path d="M12 8h.01"/>'),
  search: () => svg('<path d="m21 21-4.34-4.34"/><circle cx="11" cy="11" r="8"/>'),
  download: () => svg('<path d="M12 15V3"/><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="m7 10 5 5 5-5"/>'),
  upload: () => svg('<path d="M12 3v12"/><path d="m17 8-5-5-5 5"/><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/>'),
  pencil: () => svg('<path d="M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z"/><path d="m15 5 4 4"/>'),
  chevron: () => svg('<path d="m6 9 6 6 6-6"/>', 14),
  server: (size) => svg('<rect width="20" height="8" x="2" y="2" rx="2" ry="2"/><rect width="20" height="8" x="2" y="14" rx="2" ry="2"/><line x1="6" x2="6.01" y1="6" y2="6"/><line x1="6" x2="6.01" y1="18" y2="18"/>', size),
  laptop: (size) => svg('<path d="M18 5a2 2 0 0 1 2 2v8.526a2 2 0 0 0 .212.897l1.068 2.127a1 1 0 0 1-.9 1.45H3.62a1 1 0 0 1-.9-1.45l1.068-2.127A2 2 0 0 0 4 15.526V7a2 2 0 0 1 2-2z"/><path d="M20.054 15.987H3.946"/>', size),
  // A network folder: Lucide's folder, made smaller, joined by a stem to a jack on a line.
  netFolder: (size) => svg('<g transform="translate(3.4 -0.2) scale(0.72)" stroke-width="2.5"><path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/></g>'
    + '<path d="M12 14.4V18"/><path d="M2 21h8M14 21h8"/><rect x="10" y="18" width="4" height="4" rx="1"/>', size),
  barrel: () => svg(BARREL),
  play: () => svg('<path d="M5 5a2 2 0 0 1 3.008-1.728l11.997 6.998a2 2 0 0 1 .003 3.458l-12 7A2 2 0 0 1 5 19z"/>', 14),
  pause: () => svg('<rect x="14" y="3" width="5" height="18" rx="1"/><rect x="5" y="3" width="5" height="18" rx="1"/>', 14),
  help: () => svg('<circle cx="12" cy="12" r="10"/><path d="M9.09 9a3 3 0 0 1 5.83 1c0 2-3 3-3 3"/><path d="M12 17h.01"/>', 15),
  skull: () => svg('<path d="m12.5 17-.5-1-.5 1h1z"/><path d="M15 22a1 1 0 0 0 1-1v-1a2 2 0 0 0 1.56-3.25 8 8 0 1 0-11.12 0A2 2 0 0 0 8 20v1a1 1 0 0 0 1 1z"/><circle cx="15" cy="12" r="1"/><circle cx="9" cy="12" r="1"/>', 15),
  clearList: () => svg('<path d="M16 5H3"/><path d="M11 12H3"/><path d="M16 19H3"/><path d="m15.5 9.5 5 5"/><path d="m20.5 9.5-5 5"/>'),
  wheel: () => svg('<circle cx="12" cy="12" r="8"/><path d="M12 2v7.5"/><path d="m19 5-5.23 5.23"/><path d="M22 12h-7.5"/><path d="m19 19-5.23-5.23"/><path d="M12 14.5V22"/><path d="M10.23 13.77 5 19"/><path d="M9.5 12H2"/><path d="M10.23 10.23 5 5"/><circle cx="12" cy="12" r="2.5"/>', 24),
  // The frigate from the app icon, as a filled silhouette (drawn for this app, not from Lucide).
  frigate: () => {
    const s = svg('<rect x="9.6" y="7" width="0.9" height="18"/><rect x="20.3" y="0.6" width="0.9" height="24"/><rect x="30.4" y="3.6" width="0.9" height="21"/>'
      + '<path d="M21.2 0.4h4.2l-1.2 0.9 1.2 0.9h-4.2z"/><path d="M7.6 8.6h4.8l0.3 3h-5.4z"/><path d="M6.8 12.4h6.6l0.3 4h-7.2z"/><path d="M6.3 17.2h7.6l0.3 4.6h-8.2z"/>'
      + '<path d="M17.4 2.6h6.2l0.3 3.6h-6.8z"/><path d="M16.2 7h8.8l0.3 6h-9.4z"/><path d="M15 13.8h11l0.3 6.6h-11.6z"/><path d="M14 21.2h13l0.2 3.4h-13.4z"/>'
      + '<path d="M27.8 5.4h5.8l0.3 3.4h-6.4z"/><path d="M27.2 9.6h7l0.3 5h-7.6z"/><path d="M26.8 15.4h7.8l0.3 5.6h-8.4z"/><path d="M35.2 9.2 47 24.2h-11.4z"/>'
      + '<path d="M0 22.2h7.6v2.6H0z"/><path d="M0 25.4h40.5L48 24.2q-3.6 6.2-9.6 9.6H7.2Q1.6 31 0 25.4z"/>');
    s.setAttribute('viewBox', '0 0 48 34');
    s.setAttribute('width', 26);
    s.setAttribute('height', 18);
    s.setAttribute('fill', 'currentColor');
    s.setAttribute('stroke', 'none');
    return s;
  },
  // A datahold: two Lucide barrels, the back one cut away where the front one overlaps it.
  datahold: (size = 16) => {
    const id = `dh${++maskIds}`;
    const back = 'translate(-1.6 -0.9) scale(0.68)';
    const front = 'translate(7.3 7.4) scale(0.68)';
    return svg(`<defs><mask id="${id}" maskUnits="userSpaceOnUse" x="0" y="0" width="24" height="24"><rect width="24" height="24" fill="white"/>`
      + `<path transform="${front}" d="M16.997 21a2 2 0 001.68-.92 15.25 15.25 0 000-16.16 2 2 0 00-1.68-.92h-10a2 2 0 00-1.681.92 15.25 15.25 0 000 16.16 2 2 0 001.681.92z" fill="black" stroke="black" stroke-width="6.2"/></mask></defs>`
      + `<g mask="url(#${id})"><g transform="${back}" stroke-width="2.65">${BARREL}</g></g><g transform="${front}" stroke-width="2.65">${BARREL}</g>`, size);
  },
};

// A server's icon: a datahold's barrels, a network folder for an SFTP server, or a rack for a file server.
const serverIcon = (server, size) => (server?.kind === 'archive' ? I.datahold(size) : server?.kind === 'sftp' ? I.netFolder(size) : I.server(size));

function fmtBytes(n) {
  if (n == null) return '—';
  const u = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
  let i = 0;
  let v = n;
  while (v >= 1000 && i < u.length - 1) { v /= 1000; i++; }
  return i === 0 ? `${n} B` : `${v < 10 ? v.toFixed(1) : Math.round(v)} ${u[i]}`;
}
const fmtCount = (n) => Number(n || 0).toLocaleString();
function fmtDate(secs) {
  if (!secs) return '—';
  return new Date(secs * 1000).toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' });
}
function fmtDuration(s) {
  if (!isFinite(s) || s < 0) return '';
  if (s < 90) return `${Math.max(1, Math.round(s))}s`;
  if (s < 5400) return `${Math.round(s / 60)}m`;
  const hrs = Math.floor(s / 3600);
  return `${hrs}h ${Math.round((s - hrs * 3600) / 60)}m`;
}
const plural = (n, one, many = one + 's') => `${fmtCount(n)} ${n === 1 ? one : many}`;
const joinPath = (dir, name, archive) => (archive ? (dir === '/' ? '' : dir) + '/' + name : dir.replace(/[\\/]$/, '') + (dir.includes('\\') && !dir.includes('/') ? '\\' : '/') + name);

// Light, dark, or the system's setting (see theme.js).
const appearance = () => window.appearanceChoice();
function setAppearance(choice) {
  try {
    localStorage.setItem('appearance', choice);
  } catch {}
  window.applyAppearance();
}

function toast(text, ms = 3500) {
  const t = h('div', { class: 'toast', text });
  document.body.append(t);
  setTimeout(() => t.remove(), ms);
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

const newPane = () => ({ loc: null, path: null, listing: null, sel: new Set(), anchor: -1, back: [], status: 'idle', error: '', info: null, hits: null, query: '', searchOpen: false, searching: false, note: null, seq: 0, sort: { key: 'name', dir: 1 } });
// Stow and Transfer each keep their own two panes, so switching never loses your place.
const savedMode = () => {
  try {
    return localStorage.getItem('mode') === 'transfer' ? 'transfer' : 'stow';
  } catch {
    return 'stow';
  }
};
const S = { settings: { servers: [] }, mode: savedMode(), modes: { stow: [newPane(), newPane()], transfer: [newPane(), newPane()] }, opened: new Set(), conn: {}, jobs: new Map(), moved: new Map(), focus: 0, transfersOpen: false };
S.panes = S.modes[S.mode];
const stowMode = () => S.mode === 'stow';

const serverById = (id) => S.settings.servers.find((s) => s.id === id);
const isArchivePane = (p) => p.loc && p.loc !== 'local' && serverById(p.loc)?.kind === 'archive';
// File servers, and SFTP servers (browsed and copied to the same way, without the helper).
const isFilesPane = (p) => p.loc && p.loc !== 'local' && ['files', 'sftp'].includes(serverById(p.loc)?.kind);

// ---------------------------------------------------------------------------
// Panes
// ---------------------------------------------------------------------------

function sortedItems(p) {
  const rel = (folder) => {
    if (isArchivePane(p) || !p.path || !folder.startsWith(p.path)) return folder;
    return folder.slice(p.path.length).replace(/^[\\/]/, '');
  };
  const items = p.hits ? p.hits.map((x) => ({ ...x.item, folder: x.folder, where: rel(x.folder) })) : [...(p.listing?.items || [])];
  const { key, dir } = p.sort;
  const val = (it) => (key === 'size' ? it.size ?? -1 : key === 'date' ? (isArchivePane(p) ? it.archived : it.mtime) || 0 : it.name.toLowerCase());
  items.sort((a, b) => {
    if ((a.kind === 'dir') !== (b.kind === 'dir')) return a.kind === 'dir' ? -1 : 1;
    const x = val(a);
    const y = val(b);
    return (x < y ? -1 : x > y ? 1 : 0) * dir;
  });
  return items;
}

async function openLocation(i, loc) {
  const p = S.panes[i];
  Object.assign(p, newPane(), { loc, sort: p.sort });
  if (loc === 'local') return load(i, S.panes[i].path);
  if (!loc) return renderPane(i);
  await connect(i);
}

async function connect(i) {
  const p = S.panes[i];
  const server = serverById(p.loc);
  if (!server) return renderPane(i);
  p.status = 'connecting';
  renderPane(i);
  const info = await call('connect', { serverId: server.id }).catch((e) => ({ connected: false, message: String(e) }));
  if (p.loc !== server.id) return; // switched away meanwhile
  S.conn[server.id] = info;
  p.info = info;
  if (!info.connected) {
    p.status = 'error';
    p.error = info.message || 'Couldn’t connect.';
  } else if (info.needs) {
    p.status = 'needs';
  } else {
    return load(i, null);
  }
  renderPane(i);
}

// How far down each folder was scrolled, by location and path, so going back returns to the same spot.
const scrolls = new Map();

async function load(i, path, { push = true } = {}) {
  const p = S.panes[i];
  const prev = p.listing?.path;
  // Remember how far down this folder was scrolled, to come back to the same place.
  const shown = document.querySelector(`#pane${i} .list`);
  if (prev && shown && !p.hits && p.status === 'ready') scrolls.set(`${p.loc}\n${prev}`, shown.scrollTop);
  p.status = 'loading';
  p.hits = null;
  p.query = '';
  p.searchOpen = false;
  p.searching = false;
  p.note = null;
  renderPane(i);
  try {
    const listing = p.loc === 'local'
      ? await call('list_local', { path })
      : isFilesPane(p)
        ? await call('list_server', { serverId: p.loc, path })
        : await call('list_archive', { serverId: p.loc, path });
    if (push && prev && prev !== listing.path) p.back.push(prev);
    p.listing = listing;
    p.path = listing.path;
    p.restoreScroll = listing.path !== prev ? scrolls.get(`${p.loc}\n${listing.path}`) ?? null : null;
    p.sel.clear();
    p.anchor = -1;
    p.status = 'ready';
  } catch (e) {
    p.status = p.listing ? 'ready' : 'error';
    p.error = String(e);
    if (p.listing) toast(String(e));
    renderPane(i);
    return false;
  }
  renderPane(i);
  return true;
}

// Typing a path in place of the breadcrumbs.
function editPath(i, start = null) {
  const p = S.panes[i];
  if (p.status !== 'ready' || !p.listing) return;
  p.editingPath = true;
  p.pathDraft = start ?? p.path;
  // Select the current path so typing replaces it; after a typed / or ~, keep going.
  p.pathSelect = start == null;
  renderPane(i);
}

function stopEditPath(i) {
  const p = S.panes[i];
  if (!p.editingPath) return;
  p.editingPath = false;
  renderPane(i);
  document.querySelector(`#pane${i} .list`)?.focus();
}

async function goToTyped(i, text) {
  const p = S.panes[i];
  let path = text.trim();
  if (!path) return stopEditPath(i);
  if (isArchivePane(p)) {
    // Archive paths are always from the top; resolve . and .. here.
    const parts = [];
    for (const part of path.split('/')) {
      if (part === '..') parts.pop();
      else if (part && part !== '.') parts.push(part);
    }
    path = '/' + parts.join('/');
  }
  p.editingPath = false;
  if (!(await load(i, path))) {
    // Keep what was typed so it can be fixed.
    p.editingPath = true;
    p.pathDraft = text;
    p.pathSelect = false;
    renderPane(i);
  } else {
    document.querySelector(`#pane${i} .list`)?.focus();
  }
}

function pathField(i) {
  const p = S.panes[i];
  const menu = h('div', { class: 'path-menu', style: 'display:none' });
  let comp = null; // Tab completion: { dir, names, index, shown }
  const cache = new Map();
  const input = h('input', {
    class: 'path-input',
    value: p.pathDraft,
    spellcheck: 'false',
    autocapitalize: 'off',
    autocomplete: 'off',
    'aria-label': 'Folder path',
    oninput: () => { comp = null; showMenu(); },
    onkeydown: (e) => {
      e.stopPropagation();
      if (e.key === 'Tab') { e.preventDefault(); complete(e.shiftKey); }
      else if (comp && (e.key === 'ArrowDown' || e.key === 'ArrowUp')) { e.preventDefault(); complete(e.key === 'ArrowUp'); }
      else if (e.key === 'Enter') { e.preventDefault(); goToTyped(i, input.value); }
      else if (e.key === 'Escape') {
        e.preventDefault();
        if (comp) { comp = null; showMenu(); } else stopEditPath(i);
      }
    },
    onblur: () => setTimeout(() => stopEditPath(i), 0),
  });

  // The folders inside `dir`, as typed (listed once per folder while editing).
  const folders = async (dir) => {
    if (!cache.has(dir)) {
      const path = isArchivePane(p) ? '/' + dir.split('/').filter(Boolean).join('/') : dir;
      const l = p.loc === 'local'
        ? await call('list_local', { path })
        : isFilesPane(p)
          ? await call('list_server', { serverId: p.loc, path })
          : await call('list_archive', { serverId: p.loc, path });
      cache.set(dir, l.items.filter((it) => it.kind === 'dir').map((it) => it.name));
    }
    return cache.get(dir);
  };
  const flash = () => {
    input.classList.add('nomatch');
    setTimeout(() => input.classList.remove('nomatch'), 350);
  };
  const choose = (k) => {
    comp.index = k;
    input.value = comp.shown = comp.dir + comp.names[k] + comp.sep;
    showMenu();
  };
  function showMenu() {
    const many = comp && comp.names.length > 1;
    menu.replaceChildren(...(many ? comp.names.slice(0, 200).map((n, k) =>
      h('div', { class: `opt${k === comp.index ? ' on' : ''}`, text: n, onmousedown: (e) => { e.preventDefault(); choose(k); input.focus(); } })) : []));
    menu.style.display = many ? '' : 'none';
    menu.querySelector('.on')?.scrollIntoView({ block: 'nearest' });
  }
  // Tab: finish the folder name; with several matches, fill in what they share, then step through them.
  async function complete(back) {
    if (comp && comp.names.length > 1 && input.value === comp.shown) {
      const n = comp.names.length;
      return choose(comp.index < 0 ? (back ? n - 1 : 0) : (comp.index + (back ? n - 1 : 1)) % n);
    }
    const typed = input.value;
    const text = typed === '~' ? '~/' : typed;
    const cut = Math.max(text.lastIndexOf('/'), text.lastIndexOf('\\'));
    if (cut < 0) return flash();
    const [dir, part, sep] = [text.slice(0, cut + 1), text.slice(cut + 1), text[cut]];
    let names;
    try {
      names = await folders(dir);
    } catch {
      return flash();
    }
    if (input.value !== typed) return; // typed more meanwhile
    let matches = names.filter((n) => n.startsWith(part));
    if (!matches.length) matches = names.filter((n) => n.toLowerCase().startsWith(part.toLowerCase()));
    if (!matches.length) return flash();
    if (matches.length === 1) {
      input.value = dir + matches[0] + sep;
      comp = null;
      return showMenu();
    }
    let common = matches[0];
    for (const m of matches) while (!m.startsWith(common)) common = common.slice(0, -1);
    input.value = dir + (common.length > part.length ? common : part);
    comp = { dir, sep, names: matches, index: -1, shown: input.value };
    showMenu();
  }

  setTimeout(() => {
    input.focus();
    if (p.pathSelect) input.select();
    else input.setSelectionRange(input.value.length, input.value.length);
  }, 0);
  return h('div', { class: 'path-edit' }, input, menu);
}

// In Stow only the datahold takes drops onto its folders; in Transfer either side does.
const canDropInto = (i) => (stowMode() ? isArchivePane(S.panes[i]) : true);

const refresh = (i) => (S.panes[i].status === 'ready' ? load(i, S.panes[i].path, { push: false }) : S.panes[i].loc && openLocation(i, S.panes[i].loc));

// Stow: the left pane shows this computer or a file server, and the right pane a datahold.
// Transfer: both panes show this computer or a file server.
function locationSelect(i) {
  const p = S.panes[i];
  const files = !stowMode() || i === 0;
  const addLabel = files ? 'Add a file server…' : 'Add a datahold…';
  const label = stowMode() ? (i === 0 ? 'Source' : 'Datahold') : i === 0 ? 'Left side' : 'Right side';
  const sel = h('select', { 'aria-label': label, onchange: (e) => (e.target.value === '__add' ? addServerDialog(i, addKinds(i)) : openLocation(i, e.target.value)) });
  // SFTP servers take part in Transfer mode only (stowing needs the helper).
  const servers = S.settings.servers.filter((s) => (files ? s.kind === 'files' || (s.kind === 'sftp' && !stowMode()) : s.kind === 'archive'));
  if (files) {
    sel.append(h('option', { value: 'local', text: 'This computer' }));
    if (servers.length) {
      const g = h('optgroup', { label: 'File servers' });
      for (const s of servers) g.append(h('option', { value: s.id, text: `${s.name}${s.kind === 'sftp' ? ' (SFTP)' : ''}${s.temporary ? ' (not saved)' : ''}` }));
      sel.append(g);
    }
  } else {
    for (const s of servers) sel.append(h('option', { value: s.id, text: `${s.name}${s.temporary ? ' (not saved)' : ''}` }));
  }
  sel.append(h('option', { value: '__add', text: addLabel }));
  if (!p.loc) sel.prepend(h('option', { value: '', text: files ? 'No file server yet' : 'No datahold yet', selected: true, disabled: true }));
  else sel.value = p.loc;
  return sel;
}

// The kinds of server a pane can add: a datahold on Stow's right; otherwise file servers, and in
// Transfer mode SFTP servers too.
function addKinds(i) {
  if (stowMode() && i === 1) return ['archive'];
  return stowMode() ? ['files'] : ['files', 'sftp'];
}

function statusBadge(p) {
  if (p.loc === 'local' || !p.loc) return null;
  const [cls, text] = {
    connecting: ['busy', 'Connecting…'],
    loading: ['ok', 'Connected'],
    ready: ['ok', 'Connected'],
    needs: ['ok', 'Connected'],
    error: ['bad', 'Not connected'],
  }[p.status] || ['', ''];
  return h('span', { class: 'status' }, h('span', { class: `dot ${cls}` }), text);
}

function renderPane(i) {
  const p = S.panes[i];
  const root = document.getElementById(`pane${i}`);
  // Redrawing the same folder (say, after a click) keeps the list where it was scrolled.
  const keepScroll = p.shownPath === p.path && !p.hits ? root.querySelector('.list')?.scrollTop : null;
  p.shownPath = p.path;
  root.replaceChildren();
  // What kind of place this is: this computer, a file server, an SFTP server, or a datahold.
  const kind = p.loc === 'local' ? I.laptop(18) : p.loc ? serverIcon(serverById(p.loc), 18) : stowMode() && i === 1 ? I.datahold(18) : I.server(18);
  root.append(h('div', { class: 'pane-head' }, h('span', { class: 'type-icon' }, kind), locationSelect(i), statusBadge(p)));

  if (!p.loc) {
    root.append(stowMode() || i === 0
      ? h('div', { class: 'empty' }, h('span', { class: 'pane-icon' }, I.datahold(40)), h('h3', { text: 'No datahold yet' }),
        h('p', { text: 'A datahold is where your data is kept on the archive server. Add the server that holds it to get started.' }),
        h('button', { class: 'btn primary', text: 'Add a datahold', onclick: () => addServerDialog(i, ['archive']) }))
      : h('div', { class: 'empty' }, h('span', { class: 'pane-icon' }, I.server(40)), h('h3', { text: 'No file server yet' }),
        h('p', { text: 'Add an analysis server or other machine you can reach over SSH, to copy files to and from it.' }),
        h('button', { class: 'btn primary', text: 'Add a file server', onclick: () => addServerDialog(i, addKinds(i)) })));
    updateXferButtons();
    return;
  }
  if (p.status === 'connecting') {
    root.append(empty(`Connecting to ${serverById(p.loc)?.name}…`, 'If the server asks for a password or a two-factor code, a window will appear.', null, true));
    updateXferButtons();
    return;
  }
  if (p.status === 'error' && !p.listing) {
    root.append(empty('Can’t open this location', p.error, h('button', { class: 'btn', text: 'Try again', onclick: () => refresh(i) })));
    updateXferButtons();
    return;
  }
  if (p.status === 'needs') {
    root.append(needsView(i));
    updateXferButtons();
    return;
  }

  const archive = isArchivePane(p);
  const l = p.listing;
  // Navigation row
  const nav = h('div', { class: 'nav' },
    h('button', { class: 'icon-btn', title: 'Back', disabled: !p.back.length, onclick: () => load(i, p.back.pop(), { push: false }) }, I.back()),
    h('button', { class: 'icon-btn', title: 'Up one folder', disabled: !l?.parent || p.hits, onclick: () => load(i, l.parent) }, I.up()),
  );
  if (p.editingPath) {
    nav.append(pathField(i));
  } else {
    // Click a folder name to go there, or the empty space to type a path.
    const crumbs = h('div', { class: 'crumbs', title: 'Click to type a path (⌘L)', onclick: (e) => e.target === e.currentTarget && editPath(i) });
    (l?.crumbs || []).forEach((c, k) => {
      if (k) crumbs.append(h('span', { class: 'sep', text: '›' }));
      const project = archive && c.path === l.project;
      crumbs.append(h('span', { class: `c${project ? ' project' : ''}`, title: c.path, onclick: () => load(i, c.path) }, project ? I.barrel() : null, c.name));
    });
    nav.append(crumbs);
  }
  nav.append(h('button', { class: `icon-btn${p.searchOpen ? ' on' : ''}`, title: archive ? 'Search the datahold (⌘F)' : 'Search this folder (⌘F)', onclick: () => toggleSearch(i) }, I.search()));
  if (archive) nav.append(h('button', { class: 'icon-btn', title: 'Overboard: what was thrown out of the datahold (restorable for 30 days)', onclick: () => trashDialog(i) }, I.trash()));
  else if (p.loc === 'local') nav.append(h('button', { class: 'icon-btn', title: 'Places: your home folder, Desktop, and drives', onclick: (e) => placesMenu(i, e) }, I.compass()));
  else {
    nav.append(h('button', { class: 'icon-btn', title: 'Places: your home folder and drives on this server', onclick: (e) => placesMenu(i, e) }, I.compass()));
    nav.append(h('button', { class: 'icon-btn', title: 'Trash: what was sent to the Trash on this server', onclick: () => serverTrashDialog(i) }, I.trash()));
  }
  const frozenHere = archive && !!l?.project;
  nav.append(h('button', { class: 'icon-btn', title: frozenHere ? 'Sealed barrels can’t be changed' : 'New folder', disabled: !!p.hits || frozenHere, onclick: () => newFolder(i) }, I.newFolder()));
  nav.append(h('button', { class: 'icon-btn', title: 'Refresh', onclick: () => refresh(i) }, I.refresh()));
  root.append(nav);
  if (archive && l?.project && !p.hits) {
    root.append(h('div', { class: 'project-strip', title: 'Sealed barrels can’t be changed, only added to' }, I.barrel(), h('span', { text: 'Sealed barrel: you can add files, but not rename, move, or delete.' })));
  }
  if (p.searchOpen) root.append(searchRow(i));

  // Column headers
  const col = (key, label, cls) =>
    h('span', { class: cls, onclick: () => { p.sort = { key, dir: p.sort.key === key ? -p.sort.dir : 1 }; renderPane(i); } }, label + (p.sort.key === key ? (p.sort.dir > 0 ? ' ↑' : ' ↓') : ''));
  root.append(h('div', { class: 'cols' }, col('name', 'Name'), col('size', 'Size', 'num'), col('date', archive ? 'Archived' : 'Modified', 'num')));

  // Rows
  const items = sortedItems(p);
  const list = h('div', { class: 'list', tabindex: '0', onfocus: () => (S.focus = i), onkeydown: (e) => keys(i, e, items) });
  list.addEventListener('dragover', (e) => { if (dragFrom(e) !== null && dragFrom(e) !== i) { e.preventDefault(); list.classList.add('drop'); } });
  list.addEventListener('dragleave', () => list.classList.remove('drop'));
  list.addEventListener('drop', (e) => { list.classList.remove('drop'); dropOn(e, i, null); });
  list.addEventListener('contextmenu', (e) => { if (e.target === list) { e.preventDefault(); rowMenu(i, e, null); } });
  if (p.status === 'loading') list.append(h('div', { class: 'empty' }, h('div', { class: 'spinner' })));
  else if (p.searching) list.append(h('div', { class: 'empty' }, h('div', { class: 'spinner' }), h('p', { text: archive ? 'Searching the datahold…' : 'Searching this folder and everything in it…' })));
  else if (!items.length) list.append(h('div', { class: 'empty' }, h('p', { text: p.hits ? 'Nothing matches that search.' : 'This folder is empty.' })));
  (p.searching ? [] : items).forEach((it, k) => {
    const row = h('div', {
      class: `row ${it.kind}${p.sel.has(it.path) ? ' sel' : ''}`,
      'data-path': it.path,
      draggable: 'true',
      title: it.folder ? `${it.folder}/${it.name}` : it.is_project ? 'A sealed barrel: it can be added to, renamed, moved, or thrown overboard as a whole, but nothing in it changes' : null,
      onclick: (e) => select(i, items, k, e),
      ondblclick: () => openItem(i, it),
      oncontextmenu: (e) => { e.preventDefault(); if (!p.sel.has(it.path)) select(i, items, k, {}); rowMenu(i, e, it); },
      ondragstart: (e) => {
        if (!p.sel.has(it.path)) select(i, items, k, {});
        e.dataTransfer.setData('application/x-archive-pane', String(i));
        e.dataTransfer.effectAllowed = 'copyMove';
      },
    },
      h('span', { class: 'name' }, h('span', { class: `ic${it.is_project ? ' barrel' : ''}` }, it.is_project ? I.barrel() : it.kind === 'dir' ? I.folder() : it.kind === 'link' ? I.link() : I.file()),
        h('span', { text: it.name }), it.where ? h('span', { class: 'hit-folder', text: it.where }) : null),
      h('span', { class: 'num', text: it.kind === 'link' ? '' : fmtBytes(it.size) }),
      h('span', { class: 'num', text: fmtDate(archive ? it.archived : it.mtime) }),
    );
    if (it.kind === 'dir' && !p.hits) {
      row.addEventListener('dragover', (e) => { if (dragFrom(e) !== null && dragFrom(e) !== i && canDropInto(i)) { e.preventDefault(); e.stopPropagation(); row.classList.add('drop'); } });
      row.addEventListener('dragleave', () => row.classList.remove('drop'));
      row.addEventListener('drop', (e) => { row.classList.remove('drop'); e.stopPropagation(); dropOn(e, i, it.path); });
    }
    list.append(row);
  });
  root.append(list);

  // Footer
  const selItems = items.filter((it) => p.sel.has(it.path));
  const selSize = selItems.reduce((a, it) => a + (it.size || 0), 0);
  const selUnknown = selItems.some((it) => it.size == null);
  const count = p.hits ? plural(items.length, 'match', 'matches') : plural(items.length, 'item');
  const foot = h('div', { class: 'pane-foot' },
    h('span', { text: selItems.length ? `${selItems.length} of ${items.length} selected${selUnknown ? '' : ` · ${fmtBytes(selSize)}`}` : count }),
    p.note ? h('span', { class: 'warn-text', text: p.note }) : null,
    h('span', { class: 'spacer' }),
    l?.free_bytes != null ? h('span', { text: `${fmtBytes(l.free_bytes)} free${archive ? ' on server' : ''}` }) : null,
    archive ? h('span', { text: '· sizes are space used' }) : null,
  );
  root.append(foot);
  // Scroll only once the footer is in, or the list is still too tall and the browser cuts the
  // scroll short.
  if (keepScroll) list.scrollTop = keepScroll;
  else if (p.restoreScroll && p.status === 'ready') list.scrollTop = p.restoreScroll;
  if (p.status === 'ready') p.restoreScroll = null;
  updateXferButtons();
}

function empty(title, text, action, spin) {
  return h('div', { class: 'empty' }, spin ? h('div', { class: 'spinner' }) : null, h('h3', { text: title }), text ? h('p', { text }) : null, action);
}

function needsView(i) {
  const p = S.panes[i];
  const server = serverById(p.loc);
  const info = p.info || {};
  if (info.needs === 'helper' || info.needs === 'update') {
    const update = info.needs === 'update';
    const label = update ? 'Update the Quartermaster helper' : 'Install the Quartermaster helper';
    const btn = h('button', { class: 'btn primary', text: label });
    btn.onclick = async () => {
      btn.disabled = true;
      btn.textContent = update ? 'Updating…' : 'Installing…';
      try {
        await call('install_helper', { server });
        toast(`${update ? 'Updated' : 'Installed'}. Connecting…`);
        connect(i);
      } catch (e) {
        btn.disabled = false;
        btn.textContent = label;
        alertDialog(`Couldn’t ${update ? 'update' : 'install'} the helper`, String(e));
      }
    };
    if (update) return empty('The Quartermaster helper here needs an update', 'This version of the app needs the newer helper on this server. Updating takes a few seconds; transfers already running on the server keep going.', btn);
    return empty('This server needs the Quartermaster helper', server.kind === 'files'
      ? 'A small program in your home folder on the server lets the app browse it and run transfers there. Installing it takes a few seconds and doesn’t need an administrator.'
      : 'A small program in your home folder on the server does the checking, compressing, and protecting. Installing it takes a few seconds and doesn’t need an administrator.', btn);
  }
  if (info.needs === 'archive') {
    const btn = h('button', { class: 'btn primary', text: 'Create a datahold here' });
    btn.onclick = async () => {
      btn.disabled = true;
      try {
        await call('create_archive', { server });
        connect(i);
      } catch (e) {
        btn.disabled = false;
        alertDialog('Couldn’t create the datahold', String(e));
      }
    };
    return empty('No datahold here yet', `There’s no datahold at ${server.root} on ${server.name}.`, btn);
  }
  return empty('Connected', '');
}

function select(i, items, k, e) {
  const p = S.panes[i];
  const it = items[k];
  if (e.shiftKey && p.anchor >= 0) {
    const [a, b] = [Math.min(p.anchor, k), Math.max(p.anchor, k)];
    if (!(e.metaKey || e.ctrlKey)) p.sel.clear();
    for (let x = a; x <= b; x++) p.sel.add(items[x].path);
  } else if (e.metaKey || e.ctrlKey) {
    p.sel.has(it.path) ? p.sel.delete(it.path) : p.sel.add(it.path);
    p.anchor = k;
  } else {
    p.sel = new Set([it.path]);
    p.anchor = k;
  }
  S.focus = i;
  renderPane(i);
  focusList(i, k);
}

// Focus a pane's list, bringing row `k` (or else the first selected row) into view.
function focusList(i, k) {
  const list = document.querySelector(`#pane${i} .list`);
  if (list) {
    list.focus({ preventScroll: true });
    const row = k != null ? list.querySelectorAll('.row')[k] : list.querySelector('.row.sel');
    row?.scrollIntoView({ block: 'nearest' });
  }
}

function openItem(i, it) {
  if (it.kind === 'dir') return load(i, it.path);
  if (it.folder) {
    return load(i, it.folder).then(() => {
      S.panes[i].sel = new Set([it.path]);
      renderPane(i);
      focusList(i);
    });
  }
  if (isArchivePane(S.panes[i])) infoDialog(i, it);
}

function keys(i, e, items) {
  const p = S.panes[i];
  const idx = items.findIndex((it) => it.path === [...p.sel].at(-1));
  if ((e.key === 'ArrowDown' || e.key === 'ArrowUp') && !e.metaKey && !e.altKey) {
    e.preventDefault();
    const k = Math.max(0, Math.min(items.length - 1, idx + (e.key === 'ArrowDown' ? 1 : -1)));
    if (items[k]) select(i, items, k, { shiftKey: e.shiftKey });
  } else if (e.key === 'Enter' && idx >= 0) {
    openItem(i, items[idx]);
  } else if ((e.key === 'Backspace' && !e.metaKey) || (e.key === 'ArrowUp' && (e.metaKey || e.altKey))) {
    e.preventDefault();
    if (p.listing?.parent) load(i, p.listing.parent);
  } else if ((e.key === '/' || e.key === '~') && !e.metaKey && !e.ctrlKey) {
    // Start typing a path, as in Finder's Go to Folder.
    e.preventDefault();
    editPath(i, e.key);
  } else if (e.key === 'a' && (e.metaKey || e.ctrlKey)) {
    e.preventDefault();
    p.sel = new Set(items.map((it) => it.path));
    renderPane(i);
    focusList(i, idx >= 0 ? idx : null);
  } else if ((e.key === 'Delete' || (e.key === 'Backspace' && e.metaKey)) && p.sel.size) {
    e.preventDefault();
    trashSelection(i);
  }
}

function searchRow(i) {
  const p = S.panes[i];
  const archive = isArchivePane(p);
  const here = p.listing?.crumbs?.at(-1)?.name || 'this folder';
  const input = h('input', {
    type: 'search',
    placeholder: archive ? 'Words, or type:fastq  after:2023  >1GB  is:folder' : `Names in ${here}, or type:csv  >1GB  after:2023`,
    title: 'Every word must match. Filters: type:fastq  is:folder  is:file  after:2023-06  before:2024  >1GB  <10MB.  "Quotes" keep words together.' + (archive ? ' Words can match a file or any folder above it.' : ' * and ? work as wildcards.'),
    value: p.query,
    oninput: (e) => (p.query = e.target.value),
    onkeydown: (e) => {
      if (e.key === 'Enter') search(i, e.target.value);
      if (e.key === 'Escape') { e.stopPropagation(); closeSearch(i); }
    },
  });
  const via = { spotlight: 'found with Spotlight', locate: 'found with the file index' }[p.method];
  const status = p.searching ? 'Searching…' : p.hits ? `${plural(p.hits.length, 'match', 'matches')}${!archive && via ? ` · ${via}` : ''}` : archive ? 'Whole datahold' : `In ${here} and its subfolders`;
  const everyFile = !archive && p.hits && via
    ? h('button', { class: 'link', style: 'font-size:11px;white-space:nowrap', text: 'Search every file instead', title: 'System indexes can miss very new files and skip some folders.', onclick: () => search(i, p.query, { everyFile: true }) })
    : null;
  return h('div', { class: 'search-row' },
    h('span', { class: 'ic' }, I.search()), input,
    h('span', { class: 'scope', text: status }), everyFile,
    h('button', { class: 'icon-btn', title: p.searching ? 'Stop searching' : 'Close search (Esc)', onclick: () => closeSearch(i) }, I.x()));
}

function toggleSearch(i) {
  const p = S.panes[i];
  if (p.searchOpen) return closeSearch(i);
  if (p.status !== 'ready') return;
  p.searchOpen = true;
  renderPane(i);
  document.querySelector(`#pane${i} .search-row input`)?.focus();
}

function closeSearch(i) {
  const p = S.panes[i];
  if (p.searching && p.loc === 'local') call('search_cancel');
  p.seq++;
  const hadResults = p.hits;
  Object.assign(p, { searchOpen: false, searching: false, hits: null, query: '', note: null, method: null });
  p.sel.clear();
  renderPane(i);
  if (hadResults) focusList(i);
}

async function search(i, q, { everyFile = false } = {}) {
  const p = S.panes[i];
  q = q.trim();
  p.query = q;
  if (!q) {
    Object.assign(p, { hits: null, note: null });
    return renderPane(i);
  }
  const seq = ++p.seq;
  p.searching = true;
  p.method = null;
  p.sel.clear();
  renderPane(i);
  try {
    if (isArchivePane(p)) {
      const hits = await call('archive_search', { serverId: p.loc, query: q });
      if (seq !== p.seq) return;
      p.hits = hits;
      p.note = hits.length >= 200 ? 'Showing the first 200 matches; try a longer name.' : null;
    } else {
      const r = isFilesPane(p)
        ? await call('search_server', { serverId: p.loc, root: p.path, query: q, everyFile })
        : await call('search_local', { root: p.path, query: q, everyFile });
      if (seq !== p.seq) return;
      p.hits = r.hits;
      p.method = r.method;
      p.note = r.truncated
        ? `Showing the first ${fmtCount(r.hits.length)} matches; add another word to narrow it down.`
        : r.timed_out
          ? `Stopped after 20 seconds (${fmtCount(r.scanned)} items checked); open a subfolder to search deeper.`
          : null;
    }
  } catch (e) {
    if (seq !== p.seq || String(e).includes('cancelled')) return;
    toast(String(e));
  }
  if (seq !== p.seq) return;
  p.searching = false;
  renderPane(i);
  document.querySelector(`#pane${i} .search-row input`)?.focus();
}

async function newFolder(i) {
  const p = S.panes[i];
  const name = await promptDialog('New folder', 'Name', 'untitled folder');
  if (!name) return;
  try {
    if (p.loc === 'local') await call('local_mkdir', { path: joinPath(p.path, name, false) });
    else if (isFilesPane(p)) await call('mkdir_server', { serverId: p.loc, path: joinPath(p.path, name, false) });
    else await call('archive_mkdir', { serverId: p.loc, path: joinPath(p.path, name, true) });
    load(i, p.path, { push: false });
  } catch (e) {
    alertDialog('Couldn’t create the folder', String(e));
  }
}

// Items inside an archived project can't be renamed, moved, or deleted on
// their own; the project as a whole can.
function lockedInProject(p, items) {
  if (!items.some((it) => it.in_project)) return false;
  alertDialog('Sealed barrels can’t be changed', 'Items inside a barrel can’t be renamed or deleted on their own. You can add files to the barrel, or rename, move, or throw the whole barrel overboard.');
  return true;
}

async function renameItem(i, it) {
  const p = S.panes[i];
  if (lockedInProject(p, [it])) return;
  const name = await promptDialog('Rename', 'New name', it.name);
  if (!name || name === it.name) return;
  const parent = it.path.slice(0, it.path.lastIndexOf('/')) || '/';
  try {
    await call('archive_rename', { serverId: p.loc, from: it.path, to: joinPath(parent, name, true) });
    load(i, p.path, { push: false });
  } catch (e) {
    alertDialog('Couldn’t rename', String(e));
  }
}

// Send the selection to the Trash: this computer's, or a file server's own (kept by the helper,
// where it stays until restored or deleted for good). No popup: it can be undone. The datahold
// keeps its own Throw overboard.
async function trashSelection(i) {
  const p = S.panes[i];
  if (p.status !== 'ready' || !p.sel.size) return;
  if (isArchivePane(p)) return overboardDatahold(i);
  const items = sortedItems(p).filter((it) => p.sel.has(it.path));
  if (!items.length) return;
  return p.loc === 'local' ? trashLocal(i, items) : trashOnServer(i, items);
}

const itemsNamed = (items) => (items.length === 1 ? `“${items[0].name}”` : `${items.length} items`);
const lastName = (path) => path.replace(/[\\/]+$/, '').split(/[\\/]/).pop();

async function trashLocal(i, items) {
  try {
    await call('trash_local', { paths: items.map((it) => it.path) });
    toast(`Sent ${itemsNamed(items)} to the System Trash`);
  } catch (e) {
    alertDialog('Couldn’t send it to the System Trash', String(e));
  }
  load(i, S.panes[i].path, { push: false });
}

async function trashOnServer(i, items) {
  const p = S.panes[i];
  const server = serverById(p.loc);
  let report;
  try {
    report = await call('trash_server', { serverId: p.loc, paths: items.map((it) => it.path) });
  } catch (e) {
    alertDialog('Couldn’t send it to the Trash', String(e));
    return load(i, S.panes[i].path, { push: false });
  }
  if (report.trashed.length) toast(`Sent ${report.trashed.length === 1 ? `“${report.trashed[0].name}”` : `${report.trashed.length} items`} to the Trash on ${server.name}`);
  load(i, S.panes[i].path, { push: false });
  // Only when a server can't make a Trash for something is there a question to ask.
  const stuck = report.failed.filter((f) => f.can_delete);
  if (stuck.length) {
    const reasons = stuck.map((f) => `${lastName(f.path)}: ${f.reason}`);
    const ok = await permanentDeleteDialog({
      title: stuck.length === 1 ? `${server.name} can’t make a Trash for “${lastName(stuck[0].path)}”` : `${server.name} can’t make a Trash for ${stuck.length} items`,
      lead: 'Without a Trash, the only way to remove it is to delete it permanently.',
      lines: reasons,
      serverId: p.loc,
      paths: stuck.map((f) => f.path),
      okLabel: 'Delete permanently',
    });
    if (ok) {
      try {
        const n = await call('delete_server', { serverId: p.loc, paths: stuck.map((f) => f.path) });
        toast(`Deleted ${plural(n, 'item')} from ${server.name}`);
      } catch (e) {
        alertDialog('Couldn’t delete', String(e));
      }
      load(i, S.panes[i].path, { push: false });
    }
  }
}

// An irreversible delete on a file server. Says so plainly, counts what will go, and starts with
// Cancel selected. Resolves true if the person confirms.
function permanentDeleteDialog({ title, lead, lines = [], serverId, paths, okLabel }) {
  return new Promise((resolve) => {
    let result = false;
    const size = h('div', { class: 'note', text: 'Counting what will be deleted…' });
    const list = lines.length ? h('ul', { class: 'leave-list wrap' }, lines.slice(0, 4).map((t) => h('li', { text: t })), lines.length > 4 ? h('li', { text: `and ${lines.length - 4} more` }) : null) : null;
    const cancelB = h('button', { class: 'btn', text: 'Cancel', onclick: () => m.close() });
    const okB = h('button', { class: 'btn danger', text: okLabel, onclick: () => { result = true; m.close(); } });
    const m = modal([
      h('div', { class: 'dialog-body' }, h('h2', { text: title }),
        h('p', {}, lead, ' ', h('b', { text: 'This can’t be undone.' })), list, size),
      h('div', { class: 'dialog-foot' }, cancelB, h('span', { class: 'spacer' }), okB),
    ], { onClose: () => resolve(result) });
    setTimeout(() => cancelB.focus(), 0);
    call('measure_server', { serverId, paths })
      .then((ms) => (size.textContent = `${plural(ms.files, 'file')} in ${plural(ms.folders, 'folder')}, ${fmtBytes(ms.bytes)}, will be deleted.`))
      .catch(() => (size.textContent = 'Couldn’t count what is inside.'));
  });
}

// What is in a file server's Trash: restore items, delete chosen ones for good, or empty it.
async function serverTrashDialog(i) {
  const p = S.panes[i];
  const server = serverById(p.loc);
  const body = h('div', { class: 'dialog-body' });
  const restoreB = h('button', { class: 'btn', text: 'Restore' });
  const deleteB = h('button', { class: 'btn danger', text: 'Delete permanently' });
  const m = modal([body, h('div', { class: 'dialog-foot' }, restoreB, h('span', { class: 'spacer' }), deleteB, h('button', { class: 'btn primary', text: 'Close', onclick: () => m.close() }))], { wide: true, onClose: () => refresh(i) });
  let items = [];
  const picked = new Set();
  const boxes = new Map();
  let headText;
  let allBox;
  // Toggling a checkbox changes only what depends on it, so the list isn't rebuilt under the
  // cursor (which would lose the keyboard's place).
  const sync = () => {
    if (headText) headText.textContent = picked.size ? `${picked.size} of ${items.length} selected` : 'Select all';
    if (allBox) {
      allBox.checked = picked.size === items.length && items.length > 0;
      allBox.indeterminate = picked.size > 0 && picked.size < items.length;
    }
    restoreB.disabled = !picked.size;
    deleteB.disabled = !picked.size;
  };
  const draw = () => {
    boxes.clear();
    body.replaceChildren(h('h2', { text: `Trash on ${server.name}` }),
      h('p', { text: 'Items stay here until you restore them or delete them for good. They still use space on the server.' }));
    headText = allBox = null;
    if (!items.length) body.append(h('p', { text: 'The Trash is empty.' }));
    else {
      allBox = h('input', { type: 'checkbox', onchange: (e) => {
        picked.clear();
        for (const [id, box] of boxes) { box.checked = e.target.checked; if (e.target.checked) picked.add(id); }
        sync();
      } });
      headText = h('span', { text: 'Select all' });
      body.append(h('div', { class: 'trash-row head' }, h('label', { class: 'check' }, allBox, headText)));
    }
    for (const t of items) {
      const box = h('input', { type: 'checkbox', onchange: (e) => { e.target.checked ? picked.add(t.id) : picked.delete(t.id); sync(); } });
      boxes.set(t.id, box);
      body.append(h('label', { class: 'trash-row pick' }, box,
        h('div', { style: 'min-width:0' }, h('div', { text: `${t.name}${t.kind === 'dir' ? '/' : ''}` }), h('div', { class: 'origin', text: t.original })),
        h('span', { class: 'num', text: t.kind === 'dir' ? 'folder' : fmtBytes(t.size) }),
        h('span', { class: 'num', text: fmtDate(t.trashed_at) })));
    }
    sync();
  };
  const fill = async () => {
    try {
      items = await call('trash_list_server', { serverId: p.loc });
      picked.clear();
      draw();
    } catch (e) {
      body.replaceChildren(h('h2', { text: `Trash on ${server.name}` }), h('p', { text: String(e) }));
    }
  };
  const chosen = () => items.filter((t) => picked.has(t.id));
  restoreB.onclick = async () => {
    restoreB.disabled = true;
    const problems = [];
    let restored = 0;
    for (const t of chosen()) {
      try {
        await call('trash_restore_server', { serverId: p.loc, id: t.id });
        restored++;
      } catch (e) {
        problems.push(`${t.name}: ${e}`);
      }
    }
    if (restored) toast(`Restored ${plural(restored, 'item')}`);
    if (problems.length) alertDialog('Couldn’t restore everything', problems.join('\n'));
    fill();
  };
  const remove = async (list, ids, title, okLabel) => {
    const ok = await permanentDeleteDialog({ title, lead: `${list.length === 1 ? 'It is' : 'They are'} deleted from ${server.name}, not just from the Trash.`, serverId: p.loc, paths: list.map((t) => t.stored), okLabel });
    if (!ok) return;
    try {
      const n = await call('trash_empty_server', { serverId: p.loc, ids });
      toast(`Deleted ${plural(n, 'item')} for good`);
    } catch (e) {
      alertDialog('Couldn’t delete', String(e));
    }
    fill();
  };
  deleteB.onclick = () => {
    const list = chosen();
    const everything = list.length === items.length;
    remove(list, list.map((t) => t.id), everything ? `Empty the Trash on ${server.name}?` : `Delete ${plural(list.length, 'item')} permanently?`, 'Delete permanently');
  };
  body.append(h('div', { class: 'empty' }, h('div', { class: 'spinner' })));
  fill();
}

async function overboardDatahold(i) {
  const p = S.panes[i];
  const paths = [...p.sel];
  if (!paths.length) return;
  if (lockedInProject(p, sortedItems(p).filter((it) => p.sel.has(it.path)))) return;
  const ok = await confirmDialog(
    paths.length === 1 ? `Throw “${paths[0].split('/').pop()}” overboard?` : `Throw ${paths.length} items overboard?`,
    'You can bring them back from Overboard for 30 days. After that they are deleted from the datahold for good.',
    'Throw overboard',
    true,
  );
  if (!ok) return;
  try {
    await call('archive_trash', { serverId: p.loc, paths });
    load(i, p.path, { push: false });
  } catch (e) {
    alertDialog('Couldn’t throw it overboard', String(e));
  }
}

// ---------------------------------------------------------------------------
// Menus
// ---------------------------------------------------------------------------

function showMenu(e, entries) {
  document.querySelector('.menu')?.remove();
  if (!entries.some((en) => en && en !== '-')) return;
  const m = h('div', { class: 'menu' });
  for (const en of entries) {
    if (en === '-') m.append(h('hr'));
    else if (en) m.append(h('div', { class: en.danger ? 'danger' : '', onclick: () => { m.remove(); en.run(); } }, en.icon ? en.icon() : null, en.label));
  }
  document.body.append(m);
  const r = m.getBoundingClientRect();
  m.style.left = Math.min(e.clientX, innerWidth - r.width - 8) + 'px';
  m.style.top = Math.min(e.clientY, innerHeight - r.height - 8) + 'px';
  setTimeout(() => document.addEventListener('click', () => m.remove(), { once: true }));
}

function rowMenu(i, e, it) {
  const p = S.panes[i];
  const other = 1 - i;
  const archive = isArchivePane(p);
  if (archive) {
    // Inside a project only retrieving and info apply; projects and the
    // folders that organize them can be renamed, moved, and trashed.
    const inProject = !!p.listing?.project;
    const locked = sortedItems(p).some((x) => p.sel.has(x.path) && x.in_project);
    showMenu(e, [
      it && { label: 'Retrieve to this computer', icon: I.download, run: () => transfer(i, other) },
      it && { label: 'Get info', icon: I.info, run: () => infoDialog(i, it) },
      it && !inProject && '-',
      !inProject && { label: 'New folder', icon: I.newFolder, run: () => newFolder(i) },
      it && !locked && p.sel.size === 1 && { label: 'Rename', icon: I.pencil, run: () => renameItem(i, it) },
      it && !locked && '-',
      it && !locked && { label: 'Throw overboard', icon: I.trash, danger: true, run: () => trashSelection(i) },
    ]);
  } else {
    showMenu(e, [
      it && { label: stowMode() ? 'Send to the datahold…' : 'Transfer to the other side…', icon: stowMode() ? I.upload : I.swap, run: () => transfer(i, other) },
      { label: 'New folder', icon: I.newFolder, run: () => newFolder(i) },
      it && '-',
      it && { label: 'Throw overboard', icon: I.trash, danger: true, run: () => trashSelection(i) },
    ]);
  }
}

async function placesMenu(i, e) {
  const p = S.panes[i];
  let places;
  try {
    places = p.loc === 'local' ? await call('places') : await call('places_server', { serverId: p.loc });
  } catch (err) {
    return toast(String(err));
  }
  showMenu(e, places.map((pl) => ({ label: pl.name, run: () => load(i, pl.path) })));
}

// ---------------------------------------------------------------------------
// Transfers
// ---------------------------------------------------------------------------

// Stow: left → right sends to the datahold; right → left retrieves from it. When
// the left pane shows a file server, the transfer runs on that server.
// Transfer: either way copies files as they are, between this computer and file
// servers, running here with the data passing through this computer.
function plan(from, to) {
  if (!stowMode()) {
    const [a, b] = [S.panes[from], S.panes[to]];
    const side = (p) => p.loc === 'local' || isFilesPane(p);
    if (!side(a) || !side(b)) return { error: 'Open this computer or a file server on both sides first.' };
    if (a.loc === 'local' && b.loc === 'local') return { error: 'Choose a file server for one side. Finder is the way to copy between folders on this computer.' };
    return { direction: 'copy', serverId: '', filesId: a.loc === 'local' ? null : a.loc, toId: b.loc === 'local' ? null : b.loc };
  }
  const source = S.panes[0];
  const archive = S.panes[1];
  if (!isArchivePane(archive)) return { error: 'Open a datahold on the right first.' };
  if (source.loc !== 'local' && !isFilesPane(source)) return { error: 'Open this computer or a file server on the left first.' };
  const filesId = isFilesPane(source) ? source.loc : null;
  return from === 0 && to === 1 ? { direction: 'send', serverId: archive.loc, filesId } : { direction: 'retrieve', serverId: archive.loc, filesId };
}

function updateXferButtons() {
  const r = document.getElementById('to-right');
  const l = document.getElementById('to-left');
  if (!r) return;
  // An arrow that can't be used is dimmed, and its tooltip says why (a disabled button shows no tooltip).
  const place = (i) => (S.panes[i].loc === 'local' ? 'this computer' : serverById(S.panes[i].loc)?.name || 'the other side');
  const what = (i) => {
    const n = S.panes[i].sel.size;
    const one = n === 1 && sortedItems(S.panes[i]).find((it) => S.panes[i].sel.has(it.path));
    return one ? `“${one.name}”` : `${n} items`;
  };
  const why = (from) => {
    const to = 1 - from;
    if (!S.panes[from].loc || !S.panes[to].loc) return 'Choose a place on both sides first';
    if (S.panes[from].status !== 'ready' || S.panes[to].status !== 'ready') return 'Wait for both sides to finish opening';
    if (!S.panes[from].sel.size) return `Select files on the ${from === 0 ? 'left' : 'right'} to ${stowMode() ? (from === 0 ? 'send them to the datahold' : 'retrieve them') : `copy them ${from === 0 ? 'right' : 'left'}`}`;
    return plan(from, to).error || null;
  };
  for (const [btn, from] of [[r, 0], [l, 1]]) {
    const no = why(from);
    btn.classList.toggle('off', !!no);
    btn.setAttribute('aria-disabled', no ? 'true' : 'false');
    btn.title = no
      || (stowMode()
        ? from === 0 ? `Send ${what(0)} to ${place(1)} (you choose Copy or Move next)` : `Retrieve ${what(1)} to ${place(0)} (always a copy)`
        : `Copy or move ${what(from)} to ${place(1 - from)}`);
  }
}

// In a short window the transfer button drops the ship, so it stays clear of the location rows.
function fitXfer() {
  const main = document.querySelector('.main');
  const xfer = document.getElementById('xfer');
  if (main && xfer) xfer.classList.toggle('compact', main.clientHeight < 290);
}

// The project an archive folder is in (or is), by its path, if known; null outside projects.
function projectAt(pane, dest) {
  if (dest === pane.path) return pane.listing?.project || null;
  const it = (pane.listing?.items || []).find((x) => x.path === dest);
  return it?.is_project ? it.path : it?.in_project ? pane.listing?.project || null : null;
}

async function transfer(from, to, destPath) {
  const a = S.panes[from];
  const b = S.panes[to];
  const pl = plan(from, to);
  if (pl.error) return toast(pl.error);
  const items = sortedItems(a).filter((it) => a.sel.has(it.path));
  if (!items.length) return toast('Select something to transfer first.');
  if (b.status !== 'ready') return toast('Wait for the other side to finish opening.');
  const dest = destPath || b.path;
  if (await alreadyTransferring({ direction: pl.direction, server_id: pl.serverId, files_id: pl.filesId, to_id: pl.toId ?? null, sources: items.map((it) => it.path), dest })) return;

  // Name clashes at the destination (only knowable when sending into the open folder).
  let conflict = 'skip';
  if (dest === b.path) {
    const names = new Set((b.listing?.items || []).map((x) => x.name));
    const clashes = items.filter((it) => names.has(it.name));
    if (clashes.length) {
      conflict = await conflictDialog(clashes, pl.direction);
      if (!conflict) return;
    }
  }
  // Retrieving is always a copy; sending asks, and says which project it goes into.
  let mode = 'copy';
  let newProject = null;
  if (pl.direction === 'copy') {
    const chosen = await copyDialog(items, pl.filesId && serverById(pl.filesId), pl.toId && serverById(pl.toId), dest, pl.filesId);
    if (!chosen) return;
    mode = chosen.mode;
  } else if (pl.direction === 'send') {
    const sent = await sendDialog(items, serverById(pl.serverId), dest, pl.filesId, projectAt(b, dest));
    if (!sent) return;
    ({ mode, newProject } = sent);
  }
  if (pl.direction !== 'copy' && pl.filesId && !(await ensureRoute(pl.filesId, pl.serverId))) return;
  const req = { direction: pl.direction, server_id: pl.serverId, files_id: pl.filesId, to_id: pl.toId ?? null, sources: items.map((it) => it.path), dest, mode, conflict, new_project: newProject, relay: false };
  try {
    await call('transfer_start', { req });
  } catch (e) {
    const msg = String(e);
    if (!msg.startsWith(RELAY_OFFER)) return alertDialog('Couldn’t start the transfer', msg);
    // The file server can't reach the datahold: offer to pass the data through this computer.
    if (!(await relayDialog(serverById(pl.filesId), serverById(pl.serverId), msg.slice(RELAY_OFFER.length)))) return;
    try {
      await call('transfer_start', { req: { ...req, relay: true } });
    } catch (e2) {
      alertDialog('Couldn’t start the transfer', String(e2));
    }
  }
}

const RELAY_OFFER = 'RELAY_OFFER|';

// Items already being sent or retrieved can't be started again until that
// transfer finishes or is stopped. Says so and returns true if that's the case.
async function alreadyTransferring(req) {
  const busy = await call('transfer_busy', { req: { ...req, mode: 'copy', conflict: 'skip' } }).catch(() => null);
  if (busy) alertDialog('Already being transferred', busy);
  return !!busy;
}

function relayDialog(files, archive, why) {
  return new Promise((resolve) => {
    let result = false;
    const always = h('input', { type: 'checkbox' });
    const go = h('button', { class: 'btn primary', text: 'Send through this computer' });
    const m = modal([
      h('div', { class: 'dialog-body' },
        h('h2', { text: 'Send through this computer instead?' }),
        h('p', { text: `${files.name} can’t reach ${archive.name} directly:` }),
        h('div', { class: 'prompt-text', text: why }),
        h('p', { text: `This computer can pass the data along. ${files.name} still checks and compresses everything, and when moving, deletes files only after the datahold has verified them. Keep this computer awake with the app open until the transfer finishes; if it disconnects, the transfer pauses, and you press play to reconnect and continue where it stopped.` }),
        h('div', { class: 'field' }, h('label', { class: 'check' }, always, h('span', { text: `Always send ${files.name}’s transfers this way` })))),
      h('div', { class: 'dialog-foot' }, h('button', { class: 'btn', text: 'Cancel', onclick: () => m.close() }), go),
    ], { onClose: () => resolve(result) });
    go.onclick = async () => {
      result = true;
      if (always.checked) {
        const settings = structuredClone(S.settings);
        settings.servers.find((x) => x.id === files.id).relay = true;
        S.settings = await call('settings_save', { settings }).catch(() => S.settings);
      }
      m.close();
    };
  });
}

// With limited keys allowed, a file server needs one-time permission to reach an archive.
async function ensureRoute(filesId, archiveId) {
  // Relayed transfers don't need the file server to reach the archive.
  if (serverById(filesId)?.relay) return true;
  let st;
  try {
    st = await call('route_status', { filesId, archiveId });
  } catch (e) {
    alertDialog('Couldn’t check the connection', String(e));
    return false;
  }
  // Without limited keys, the file server signs in when the transfer starts.
  if (st.same_machine || st.ready || !st.keys) return true;
  const files = serverById(filesId);
  const archive = serverById(archiveId);
  return new Promise((resolve) => {
    let result = false;
    const body = h('div', { class: 'dialog-body' },
      h('h2', { text: `Let ${files.name} send to ${archive.name} directly?` }),
      h('p', { text: `Transfers between ${files.name} and the datahold will run on ${files.name} itself, so the data doesn’t pass through this computer and you can close the app while they run.` }),
      h('div', { class: 'note', text: `To allow this, ${files.name} gets its own key that the datahold accepts only for adding and reading data. It can’t delete, rename, or sign in to the archive server. You can remove it later in Settings.` }));
    const setup = h('button', { class: 'btn primary', text: 'Set up' });
    const m = modal([body, h('div', { class: 'dialog-foot' }, h('button', { class: 'btn', text: 'Cancel', onclick: () => m.close() }), setup)], { onClose: () => resolve(result) });
    setup.onclick = async () => {
      setup.disabled = true;
      setup.textContent = 'Setting up…';
      try {
        await call('route_setup', { filesId, archiveId });
        result = true;
        m.close();
      } catch (e) {
        setup.disabled = false;
        setup.textContent = 'Try again';
        body.append(h('p', { class: 'bad-text', text: String(e) }));
      }
    };
  });
}

function dragFrom(e) {
  const t = [...(e.dataTransfer?.types || [])];
  if (!t.includes('application/x-archive-pane')) return null;
  // The value isn't readable during dragover; infer the source as the other pane.
  return S.dragging ?? null;
}
document.addEventListener('dragstart', (e) => {
  const pane = e.target.closest?.('.pane');
  S.dragging = pane ? Number(pane.id.slice(4)) : null;
  S.dragSource = S.dragging;
});
document.addEventListener('dragend', () => {
  S.dragging = null;
  S.dragEnded = Date.now();
});

function dropOn(e, to, folder) {
  e.preventDefault();
  const from = Number(e.dataTransfer.getData('application/x-archive-pane'));
  if (Number.isNaN(from) || from === to) return;
  S.paneDropped = Date.now();
  transfer(from, to, folder || undefined);
}

// Whether a drag position (physical pixels, from the app's drag events) is over the datahold pane.
function overDatahold(position) {
  if (!position) return false;
  return !!document.elementFromPoint(position.x / devicePixelRatio, position.y / devicePixelRatio)?.closest('#pane1');
}

// The dotted box over a pane while files from Finder hover over it: "Stow to …"
// on the datahold, "Copy to …" on a file server in Transfer mode.
function dropHint(i, title, icon) {
  for (const box of document.querySelectorAll('.stow-hint')) if (i == null || box.parentElement.id !== `pane${i}`) box.remove();
  if (i == null) return;
  const pane = document.getElementById(`pane${i}`);
  if (pane.querySelector('.stow-hint')) return;
  const p = S.panes[i];
  const folder = isArchivePane(p) ? (p.path && p.path !== '/' ? p.listing?.crumbs?.slice(1).map((c) => c.name).join(' › ') : null) : p.path;
  pane.append(h('div', { class: 'stow-hint' },
    h('span', { class: 'pane-icon' }, icon),
    h('div', { class: 'stow-title', text: title }),
    folder ? h('div', { class: 'stow-sub', text: `in ${folder}` }) : null));
}
const stowHint = (show) => (show ? dropHint(1, `Stow to ${serverById(S.panes[1].loc)?.name || 'the datahold'}`, I.datahold(36)) : dropHint(null));

// The pane under a drag position (physical pixels), if any.
function paneUnder(position) {
  const pane = position && document.elementFromPoint(position.x / devicePixelRatio, position.y / devicePixelRatio)?.closest('.pane');
  return pane ? Number(pane.id.slice(4)) : null;
}

// In the Mac app the window takes every drop itself, so a drag between the
// panes arrives as the app's drop event instead. Find the pane (and folder
// row) under the drop point, and transfer only if it's the other side.
function paneDrop(position) {
  const from = S.dragSource;
  if (from == null || !position || Date.now() - (S.paneDropped || 0) < 1500) return;
  const el = document.elementFromPoint(position.x / devicePixelRatio, position.y / devicePixelRatio);
  const pane = el?.closest('.pane');
  if (!pane) return;
  const to = Number(pane.id.slice(4));
  if (to === from) return;
  // As in the page's own drops, a folder row in the datahold is the destination.
  const row = el.closest('.row.dir');
  const folder = canDropInto(to) && row && !S.panes[to].hits ? row.dataset.path : undefined;
  transfer(from, to, folder);
}

// Files dragged in from Finder or Explorer go to the folder open in the datahold pane.
function stowDrop(type, paths, position) {
  const p = S.panes[1];
  const ready = isArchivePane(p) && p.status === 'ready';
  const over = overDatahold(position);
  if (type === 'enter' || type === 'over') return stowHint(ready && over);
  stowHint(false);
  if (type !== 'drop' || !paths?.length || !over) return;
  if (!ready) return toast('Open a datahold on the right, then drop files onto it.');
  const names = new Set((p.listing?.items || []).map((x) => x.name));
  const go = async () => {
    // Folders become barrels; loose files need one (the send dialog asks).
    if (await alreadyTransferring({ direction: 'send', server_id: p.loc, files_id: null, sources: paths, dest: p.path })) return;
    const kinds = await call('local_kinds', { paths });
    const items = paths.map((path, k) => ({ path, name: path.split(/[\\/]/).pop(), kind: kinds[k] }));
    let conflict = 'skip';
    const clashes = items.filter((it) => names.has(it.name));
    if (clashes.length && !(conflict = await conflictDialog(clashes, 'send'))) return;
    const sent = await sendDialog(items, serverById(p.loc), p.path, null, p.listing?.project || null);
    if (!sent) return;
    await call('transfer_start', { req: { direction: 'send', server_id: p.loc, sources: paths, dest: p.path, mode: sent.mode, conflict, new_project: sent.newProject } })
      .catch((e) => alertDialog('Couldn’t start the transfer', String(e)));
  };
  go();
}

// Transfer mode: files dragged in from Finder are copied into the folder open on a file server.
function transferDrop(type, paths, position) {
  const i = paneUnder(position);
  const p = i == null ? null : S.panes[i];
  const ready = !!p && isFilesPane(p) && p.status === 'ready';
  if (type === 'enter' || type === 'over') return ready ? dropHint(i, `Copy to ${serverById(p.loc).name}`, serverIcon(serverById(p.loc), 36)) : dropHint(null);
  dropHint(null);
  if (type !== 'drop' || !paths?.length || i == null) return;
  if (p.loc === 'local') return toast('Those files are already on this computer. Drop them onto a file server to copy them there.');
  if (!ready) return toast('Wait for the server to finish opening, then drop the files again.');
  const go = async () => {
    const req = { direction: 'copy', server_id: '', files_id: null, to_id: p.loc, sources: paths, dest: p.path };
    if (await alreadyTransferring(req)) return;
    const kinds = await call('local_kinds', { paths });
    const items = paths.map((path, k) => ({ path, name: path.split(/[\\/]/).pop(), kind: kinds[k] }));
    let conflict = 'skip';
    const names = new Set((p.listing?.items || []).map((x) => x.name));
    const clashes = items.filter((it) => names.has(it.name));
    if (clashes.length && !(conflict = await conflictDialog(clashes, 'copy'))) return;
    const chosen = await copyDialog(items, null, serverById(p.loc), p.path, null);
    if (!chosen) return;
    await call('transfer_start', { req: { ...req, mode: chosen.mode, conflict, new_project: null, relay: false } })
      .catch((e) => alertDialog('Couldn’t start the transfer', String(e)));
  };
  go();
}

// The Dock is redrawn with every progress update, several times a second. A redraw between
// pressing and releasing the mouse replaces the button under it and the click is lost (the skull
// would seem to do nothing), so while the mouse is down in the Dock its redraw waits.
let dockPressedAt = 0;
let dockDirty = false;
document.addEventListener('pointerdown', (e) => {
  if (e.target.closest?.('#transfers')) dockPressedAt = Date.now();
}, true);
const dockReleased = () => {
  if (!dockPressedAt) return;
  dockPressedAt = 0;
  // After the click has been handled.
  if (dockDirty) setTimeout(renderTransfers, 0);
};
document.addEventListener('pointerup', dockReleased, true);
document.addEventListener('pointercancel', dockReleased, true);

// A status bar that sums up transfers in one line; clicking it shows each one.
function renderTransfers() {
  // (A press whose release was never seen, say outside the window, stops holding it up after 2 s.)
  if (dockPressedAt && Date.now() - dockPressedAt < 2000) {
    dockDirty = true;
    return;
  }
  dockDirty = false;
  const box = document.getElementById('transfers');
  const jobs = [...S.jobs.values()];
  const open = S.transfersOpen && jobs.length > 0;
  box.replaceChildren();
  box.classList.toggle('open', open);
  if (open) box.append(transferDetails(jobs));
  box.append(statusBar(jobs, open));
}

function statusBar(jobs, open) {
  const running = jobs.filter((j) => j.state === 'running');
  const queued = jobs.filter((j) => j.state === 'queued').length;
  const waiting = jobs.filter((j) => j.state === 'waiting').length;
  const paused = jobs.filter((j) => j.state === 'paused').length;
  const problems = jobs.filter((j) => j.state === 'failed' || j.state === 'interrupted').length;
  const finished = jobs.filter(isFinished).length;
  const parts = [];
  let pct = null;
  if (running.length) {
    const sum = (k) => running.reduce((a, j) => a + (j[k] || 0), 0);
    const [done, total, rate] = [sum('done'), sum('total'), sum('rate')];
    if (total) pct = Math.floor((done / total) * 100);
    const eta = rate > 0 && total > done ? fmtDuration((total - done) / rate) : '';
    parts.push(h('span', { class: 'what', text: running.length === 1 ? running[0].title : `${running.length} transfers running` }));
    if (pct != null) parts.push(`${pct}%`);
    if (rate) parts.push(`${fmtBytes(rate)}/s`);
    if (eta) parts.push(`about ${eta} left`);
  }
  if (queued) parts.push(`${queued} queued`);
  if (waiting) parts.push(h('span', { class: 'warn-text', text: `${waiting} awaiting clearance (sign in to continue)` }));
  if (paused) parts.push(`${paused} paused`);
  if (problems) parts.push(h('span', { class: 'bad-text', text: `${problems} with problems` }));
  if (!running.length && !queued && !waiting && !paused && finished > problems) parts.push(`${finished - problems} finished`);
  if (!jobs.length) parts.push('Nothing at the dock');
  const summary = h('span', { class: 'summary' });
  parts.forEach((part, k) => summary.append(...(k ? [h('span', { class: 'dot-sep', text: ' · ' })] : []), part));
  const toggle = () => {
    if (!jobs.length) return;
    S.transfersOpen = !S.transfersOpen;
    renderTransfers();
  };
  return h('div', {
    class: `statusbar${jobs.length ? '' : ' idle'}`,
    role: 'button',
    tabindex: jobs.length ? '0' : null,
    title: jobs.length ? (open ? 'Hide the dock' : 'Show the dock') : null,
    onclick: toggle,
    onkeydown: (e) => (e.key === 'Enter' || e.key === ' ') && (e.preventDefault(), toggle()),
  },
    pct != null ? h('div', { class: 'overall', style: `width:${pct}%` }) : null,
    running.length ? h('span', { class: 'ic' }, jobIcon(running[0])) : null,
    summary,
    h('span', { class: 'spacer' }),
    jobs.length ? h('span', { class: `chev${open ? ' open' : ''}` }, I.chevron()) : null,
  );
}

function transferDetails(jobs) {
  const count = (states) => jobs.filter((j) => states.includes(j.state)).length;
  const panel = h('div', { class: 'transfers-panel' });
  panel.append(h('div', { class: 'transfers-head' },
    h('b', { text: 'Dock' }), h('span', { class: 'spacer' }),
    control(I.play, 'Play all paused transfers', () => call('transfers_pause_all', { pause: false }), !count(['paused'])),
    control(I.pause, 'Pause all transfers', () => call('transfers_pause_all', { pause: true }), !count(['queued', 'running', 'waiting'])),
    control(I.clearList, 'Clear finished transfers', async () => {
      await call('transfers_clear');
      for (const j of jobs) if (isFinished(j)) S.jobs.delete(j.id);
      renderTransfers();
    }, !jobs.some(isFinished)),
  ));
  const list = h('div', { class: 'jobs' });
  for (const j of jobs.slice().reverse()) {
    const pct = j.total ? Math.floor((j.done / j.total) * 100) : 0;
    let right = '';
    if (j.state === 'queued') right = 'Queued';
    else if (j.state === 'running') {
      const eta = j.rate > 0 && j.total ? fmtDuration((j.total - j.done) / j.rate) : '';
      right = j.total ? `${pct}% · ${fmtBytes(j.rate)}/s${eta ? ` · about ${eta} left` : ''}` : j.message;
    } else if (j.state === 'paused') right = j.total ? `Paused at ${pct}%` : 'Paused';
    else if (j.state === 'done') right = '✓ Done';
    else if (j.state === 'failed') right = '✕ Problems';
    else if (j.state === 'interrupted') right = 'Interrupted';
    // A server job waits for a sign-in. (A transfer from this computer that loses its connection is paused.)
    else if (j.state === 'waiting') right = h('button', { class: 'btn small', text: 'Sign in', onclick: (e) => signInAgain(j, e.target) });
    else right = 'Stopped';
    const where = j.relay ? (j.state === 'running' ? 'Through this computer (keep the app open) · ' : 'Through this computer · ') : j.server ? `On ${j.server} · ` : '';
    const sub = where + jobDetail(j);
    list.append(h('div', { class: `job ${j.state}` },
      h('span', { class: 'ic' }, jobIcon(j)),
      h('div', { style: 'min-width:0' },
        h('div', { class: 'title' }, h('span', { class: 'tag', text: jobTag(j) }), j.title),
        j.state === 'running' || j.state === 'paused' ? h('div', { class: 'bar' }, h('div', { style: `width:${pct}%` })) : null,
        h('div', { class: 'sub', title: sub }, j.state === 'done' ? [h('span', { class: 'ok-text', text: j.direction === 'copy' ? (j.size_only ? 'Size matches ✓' : 'Checksums match ✓') : 'Tally matches the manifest ✓' }), ' · '] : null, sub),
      ),
      h('span', { class: 'right' }, right, j.problems?.length ? h('button', { class: 'link', style: 'margin-left:8px', text: 'Details', onclick: () => problemsDialog(j) }) : null),
      h('span', { class: 'job-actions' }, ...jobControls(j)),
    ));
  }
  panel.append(list);
  return panel;
}

const jobIcon = (j) => (j.direction === 'send' ? I.upload() : j.direction === 'copy' ? I.swap() : I.download());
const jobTag = (j) => ({ send: 'Stow', retrieve: 'Retrieve', copy: 'Transfer' })[j.direction] || '';

// Abandon ship in Transfer mode: stop, and delete what the transfer copied (plain wording).
async function stopCopyDialog(j) {
  const n = j.created?.length || 0;
  const names = (j.created || []).map((p) => `“${p.replace(/\/$/, '').split(/[\\/]/).pop()}”`).join(', ');
  const text = n
    ? `This stops the transfer and deletes what it copied so far: ${names}. Nothing is removed from where it came from; originals are only deleted once everything has arrived.`
    : 'This stops the transfer. It hasn’t created anything that needs removing, and nothing is removed from where it came from.';
  if (!(await confirmDialog('Abandon ship?', text, 'Abandon ship', true))) return;
  await call('transfer_abandon', { id: j.id });
}

// Abandon ship: stop a transfer and remove what it created, as if it never started.
async function abandonDialog(j) {
  if (j.direction === 'copy') return stopCopyDialog(j);
  const n = j.created?.length || 0;
  const names = (j.created || []).map((p) => `“${p.replace(/\/$/, '').split('/').pop()}”`).join(', ');
  const from = j.server || 'this computer';
  const text = j.direction === 'send'
    ? n
      ? `This stops the transfer and throws its unfinished barrel${n > 1 ? 's' : ''} overboard: ${names} can be brought back from Overboard for 30 days. Nothing is removed from ${from}; originals are only deleted once a barrel is complete.`
      : `This stops the transfer. It hasn’t started a barrel of its own (it was adding to an existing barrel, or hadn’t stowed anything yet), so nothing goes overboard. Nothing is removed from ${from}.`
    : n
      ? `This stops the transfer and deletes what it retrieved so far: ${names} on ${from}. The datahold keeps everything.`
      : 'This stops the transfer. It hasn’t created anything that needs removing, and the datahold keeps everything.';
  if (!(await confirmDialog('Abandon ship?', text, 'Abandon ship', true))) return;
  toast('Abandoning ship…');
  await call('transfer_abandon', { id: j.id });
}

// A small icon button for the transfer list. The action's result arrives as a transfer event.
function control(icon, title, action, disabled = false) {
  return h('button', {
    class: 'icon-btn', title, 'aria-label': title, disabled,
    onclick: async (e) => {
      e.stopPropagation();
      try {
        await action();
      } catch (err) {
        alertDialog('Couldn’t do that', String(err));
      }
    },
  }, icon());
}

// Queued: cancel. Moving: pause and abandon ship. Paused: play and abandon ship. Finished: close.
function jobControls(j) {
  const cmd = (name) => () => call(name, { id: j.id });
  const stop = control(I.skull, 'Abandon ship: stop, and undo what this transfer did', () => abandonDialog(j));
  const close = control(I.x, 'Close', async () => {
    await call('transfer_dismiss', { id: j.id });
    S.jobs.delete(j.id);
    renderTransfers();
  });
  switch (j.state) {
    case 'queued': return [control(I.x, 'Cancel this transfer', cmd('transfer_cancel'))];
    case 'running':
    case 'waiting': return [control(I.pause, 'Pause (continue later where it stopped)', cmd('transfer_pause')), stop];
    case 'paused': return [control(I.play, 'Continue where it stopped', cmd('transfer_resume')), stop];
    // A server job stopped by a restart can continue too.
    case 'interrupted': return j.server ? [control(I.play, 'Continue where it stopped', cmd('transfer_resume')), close] : [close];
    default: return [close];
  }
}

// The line under a transfer's title. Sending shows as stowing; paused jobs wait for a sign-in.
// A running transfer whose byte count hasn't moved for a few seconds (a large
// file being checked, say) shows how long it's been, so it never looks frozen.
function jobDetail(j) {
  if (j.state === 'waiting' && j.link) return 'Awaiting clearance: sign in to continue where it stopped.';
  if (j.state !== 'running') return j.message;
  const doing = j.direction === 'send' && j.message === 'Transferring' ? 'Stowing the folder' : j.message;
  if (!S.moved.has(j.id)) S.moved.set(j.id, Date.now());
  const still = (Date.now() - S.moved.get(j.id)) / 1000;
  const since = still >= 3 ? ` · ${fmtDuration(still)}` : '';
  return (j.current ? `${doing} · ${j.current}` : doing) + since;
}

function isFinished(j) {
  return ['done', 'failed', 'cancelled', 'interrupted'].includes(j.state);
}

// A server job paused because its connection to the archive closed.
async function signInAgain(j, btn) {
  btn.disabled = true;
  btn.textContent = 'Signing in…';
  try {
    await call('job_sign_in', { id: j.id });
    toast(`Signed in. ${j.server} is continuing the transfer.`);
  } catch (e) {
    btn.disabled = false;
    btn.textContent = 'Sign in';
    alertDialog('Couldn’t sign in', String(e));
  }
}

function onTransfer(j) {
  const before = S.jobs.get(j.id);
  if (!before || before.done !== j.done || before.state !== j.state) S.moved.set(j.id, Date.now());
  S.jobs.set(j.id, j);
  renderTransfers();
  // Finished or paused partway: files may have been added or (with Move) removed.
  if ((isFinished(j) || j.state === 'paused') && before?.state !== j.state) {
    // Show the results in whichever panes are looking at the affected places.
    S.panes.forEach((p, i) => p.status === 'ready' && !p.hits && load(i, p.path, { push: false }));
  }
}

// ---------------------------------------------------------------------------
// Dialogs
// ---------------------------------------------------------------------------

function modal(content, { wide = false, onClose } = {}) {
  const dlg = h('div', { class: `dialog${wide ? ' wide' : ''}`, role: 'dialog' }, content);
  const ov = h('div', { class: 'overlay' }, dlg);
  const close = () => { ov.remove(); document.querySelectorAll('.tip').forEach((t) => t.remove()); document.removeEventListener('keydown', esc); onClose?.(); };
  const esc = (e) => e.key === 'Escape' && close();
  document.addEventListener('keydown', esc);
  document.body.append(ov);
  return { close, dlg };
}

function alertDialog(title, text) {
  const m = modal([h('div', { class: 'dialog-body' }, h('h2', { text: title }), h('p', { text, style: 'white-space:pre-wrap' })), h('div', { class: 'dialog-foot' }, h('button', { class: 'btn primary', text: 'OK', onclick: () => m.close() }))]);
}

function confirmDialog(title, text, okLabel = 'OK', danger = false) {
  return new Promise((resolve) => {
    let result = false;
    const m = modal(
      [h('div', { class: 'dialog-body' }, h('h2', { text: title }), h('p', { text })),
        h('div', { class: 'dialog-foot' }, h('button', { class: 'btn', text: 'Cancel', onclick: () => m.close() }), h('button', { class: `btn ${danger ? 'danger' : 'primary'}`, text: okLabel, onclick: () => { result = true; m.close(); } }))],
      { onClose: () => resolve(result) },
    );
  });
}

function promptDialog(title, label, value = '') {
  return new Promise((resolve) => {
    let result = null;
    const input = h('input', { value, onkeydown: (e) => e.key === 'Enter' && ok() });
    const err = h('div', { class: 'hint bad-text' });
    const ok = () => {
      const v = input.value.trim();
      if (!v || v.includes('/') || v === '.' || v === '..') { err.textContent = 'Enter a name without slashes.'; return; }
      result = v;
      m.close();
    };
    const m = modal(
      [h('div', { class: 'dialog-body' }, h('h2', { text: title }), h('div', { class: 'field' }, h('label', { text: label }), input, err)),
        h('div', { class: 'dialog-foot' }, h('button', { class: 'btn', text: 'Cancel', onclick: () => m.close() }), h('button', { class: 'btn primary', text: 'OK', onclick: ok }))],
      { onClose: () => resolve(result) },
    );
    input.focus();
    input.setSelectionRange(0, value.lastIndexOf('.') > 0 ? value.lastIndexOf('.') : value.length);
  });
}

function conflictDialog(clashes, direction) {
  return new Promise((resolve) => {
    let result = null;
    const where = direction === 'send' ? 'in the datahold folder' : direction === 'copy' ? 'in the destination folder' : 'in the folder on this computer';
    const names = clashes.slice(0, 5).map((c) => c.name).join(', ') + (clashes.length > 5 ? `, and ${clashes.length - 5} more` : '');
    const choose = (v) => () => { result = v; m.close(); };
    const m = modal(
      [h('div', { class: 'dialog-body' },
        h('h2', { text: clashes.length === 1 ? `“${clashes[0].name}” already exists` : `${clashes.length} items already exist` }),
        h('p', { text: `Items with these names are already ${where}: ${names}. ${direction === 'copy' ? 'Keep both saves the new one next to it under a new name. Skip and Replace leave files with the same size and date alone.' : 'Identical files are recognized and never sent twice.'} What should happen to files that differ?` }),
        direction === 'send' ? h('div', { class: 'note', text: 'Sealed barrels can’t be changed, so files already there stay as they are.' }) : null),
        h('div', { class: 'dialog-foot' },
          h('button', { class: 'btn', text: 'Cancel', onclick: () => m.close() }), h('span', { class: 'spacer' }),
          h('button', { class: 'btn', text: 'Skip them', onclick: choose('skip') }),
          h('button', { class: 'btn', text: 'Keep both', onclick: choose('keep-both') }),
          direction === 'send' ? null : h('button', { class: 'btn primary', text: 'Replace', onclick: choose('replace') }))],
      { onClose: () => resolve(result) },
    );
  });
}

// Confirm a send, choosing Copy or Move, and (for loose files outside any
// project) a new project's name. Resolves to { mode, newProject } or null.
// Each choice shows a few words; the details are behind help icons.
function sendDialog(items, server, dest, filesId = null, project = null) {
  return new Promise((resolve) => {
    let result = null;
    const loose = !project && items.some((it) => it.kind !== 'dir');
    const nameInput = h('input', { value: items.length === 1 ? items[0].name.replace(/\.[^.]*$/, '') : `Files ${new Date().toISOString().slice(0, 10)}` });
    const projectRow = project
      ? h('div', { class: 'help-row' }, `Adds to the barrel “${project.split('/').pop()}”`,
        help('Files already in the barrel stay as they are: a sealed barrel can be added to, but not changed.'))
      : loose
        ? h('div', { class: 'field' }, h('label', { class: 'help-row' }, 'New barrel name',
          help('Loose files are stowed in a barrel, so they get a new one. A barrel holds one project’s files and is sealed once stowed: you can add files to it, or rename, move, or throw it overboard as a whole.')), nameInput)
        : h('div', { class: 'help-row' }, items.length === 1 ? 'Becomes a new barrel' : 'Each folder becomes a new barrel',
          help('A barrel holds one project’s files and is sealed once stowed. You can add files to it, or rename, move, or throw it overboard as a whole, but nothing inside it can be changed.'));
    let mode = S.settings.default_mode === 'move' ? 'move' : 'copy';
    const source = filesId ? serverById(filesId) : null;
    const from = source ? source.name : 'this computer';
    const what = items.length === 1 ? `“${items[0].name}”` : `${items.length} items`;
    const size = h('span', { text: 'Counting files…' });
    const warning = h('p', { class: 'warn-text send-warning', text: 'Deletes the originals once everything is archived' });
    const copyB = h('button', { text: 'Copy', onclick: () => { mode = 'copy'; show(); } });
    const moveB = h('button', { text: 'Move', onclick: () => { mode = 'move'; show(); } });
    const go = h('button', {
      class: 'btn primary',
      onclick: () => {
        const name = nameInput.value.trim();
        if (loose && (!name || name.includes('/'))) return nameInput.focus();
        result = { mode, newProject: loose ? name : null };
        m.close();
      },
    });
    const [whereShort, whereLong] = !source
      ? ['You can stop and continue later', 'You can close the app partway through and start it again later. Files already sent stay safe, and the rest are sent next time.']
      : source.relay
        ? ['Keep this computer awake', `This goes from ${from} through this computer, so keep it awake with the app open until it finishes. If it disconnects, it pauses, and you press play to reconnect and continue where it stopped.`]
        : [`Runs on ${from}`, `This runs on ${from}, so you can close the app while it works. Reopen the app any time to check on it.`];
    const show = () => {
      copyB.classList.toggle('on', mode === 'copy');
      moveB.classList.toggle('on', mode === 'move');
      // Its space stays reserved, so the dialog doesn't change size with the toggle.
      warning.style.visibility = mode === 'move' ? 'visible' : 'hidden';
      go.textContent = mode === 'move' ? 'Move' : 'Copy';
    };
    const m = modal(
      [h('div', { class: 'dialog-body' },
        h('h2', { text: `Send ${what} to ${server.name}` }),
        h('p', {}, size, ` · to ${dest === '/' ? 'the top level' : dest}`),
        projectRow,
        h('div', { class: 'help-row' }, h('div', { class: 'seg send-mode' }, copyB, moveB),
          help(`Copy keeps the originals on ${from}. Move deletes them from ${from}, but only once the whole transfer is archived and every checksum matches. Pausing or abandoning it before then leaves them untouched.`)),
        warning),
        h('div', { class: 'dialog-foot' },
          h('span', { class: 'help-row where' }, whereShort, help(whereLong)), h('span', { class: 'spacer' }),
          h('button', { class: 'btn', text: 'Cancel', onclick: () => m.close() }), go)],
      { onClose: () => resolve(result) },
    );
    show();
    call(filesId ? 'measure_server' : 'measure_local', filesId ? { serverId: filesId, paths: items.map((it) => it.path) } : { paths: items.map((it) => it.path) }).then((ms) => (size.textContent = `${plural(ms.files, 'file')} in ${plural(ms.folders, 'folder')}, ${fmtBytes(ms.bytes)}`));
  });
}

// Confirm a plain copy in Transfer mode, choosing Copy or Move. Resolves to { mode } or null.
function copyDialog(items, from, to, dest, filesId) {
  return new Promise((resolve) => {
    let result = null;
    let mode = S.settings.default_mode === 'move' ? 'move' : 'copy';
    const fromName = from ? from.name : 'this computer';
    const toName = to ? to.name : 'this computer';
    const what = items.length === 1 ? `“${items[0].name}”` : `${items.length} items`;
    const size = h('span', { text: 'Counting files…' });
    const warning = h('p', { class: 'warn-text send-warning', text: `Deletes the originals from ${fromName} once everything has arrived and been checked` });
    const copyB = h('button', { text: 'Copy', onclick: () => { mode = 'copy'; show(); } });
    const moveB = h('button', { text: 'Move', onclick: () => { mode = 'move'; show(); } });
    const go = h('button', { class: 'btn primary', onclick: () => { result = { mode }; m.close(); } });
    const show = () => {
      copyB.classList.toggle('on', mode === 'copy');
      moveB.classList.toggle('on', mode === 'move');
      // Its space stays reserved, so the dialog doesn't change size with the toggle.
      warning.style.visibility = mode === 'move' ? 'visible' : 'hidden';
      go.textContent = mode === 'move' ? 'Move' : 'Copy';
    };
    const m = modal(
      [h('div', { class: 'dialog-body' },
        h('h2', { text: `Transfer ${what} to ${toName}` }),
        h('p', {}, size, ` · to ${dest}`),
        h('div', { class: 'help-row' }, h('div', { class: 'seg send-mode' }, copyB, moveB),
          help(`Copy keeps the originals on ${fromName}. Move deletes them from ${fromName}, but only once every file has arrived and every checksum matches. Pausing or abandoning it before then leaves them untouched.`)),
        warning),
        h('div', { class: 'dialog-foot' },
          h('span', { class: 'help-row where' }, 'Keep the app open', help('This runs in the app, with the data passing through this computer. If the connection drops it pauses, and you press play to reconnect and continue where it stopped. If you close the app, it comes back paused.')),
          h('span', { class: 'spacer' }),
          h('button', { class: 'btn', text: 'Cancel', onclick: () => m.close() }), go)],
      { onClose: () => resolve(result) },
    );
    show();
    call(filesId ? 'measure_server' : 'measure_local', filesId ? { serverId: filesId, paths: items.map((it) => it.path) } : { paths: items.map((it) => it.path) }).then((ms) => (size.textContent = `${plural(ms.files, 'file')} in ${plural(ms.folders, 'folder')}, ${fmtBytes(ms.bytes)}`));
  });
}

// Closing a window (or quitting) while transfers from this computer are moving. Says what that
// interrupts and lets you pause them (to continue later, in another window or the next time you
// open one), abandon ship (undo them), or stay. There is no plain stop: a transfer is either
// paused, to be finished, or undone. `info` comes from the app: { transfers, paused, servers, connections }.
let leaving = false;
function leaveDialog(info, quitting) {
  if (leaving) return;
  leaving = true;
  const n = info.transfers.length;
  const list = h('ul', { class: 'leave-list' }, info.transfers.slice(0, 4).map((t) => h('li', { text: t })), n > 4 ? h('li', { text: `and ${n - 4} more` }) : null);
  const lines = [
    n === 1 ? 'A transfer from this computer is in progress:' : `${n} transfers from this computer are in progress:`,
  ];
  const where = quitting ? 'the next time you open the app' : 'in another window, or the next time you open one';
  const notes = [
    h('p', {}, h('b', { text: 'Pause' }), ` keeps ${n === 1 ? 'it' : 'them'}: ${n === 1 ? 'it continues' : 'they continue'} where ${n === 1 ? 'it' : 'they'} stopped when you play ${n === 1 ? 'it' : 'them'} again (${where}).`),
    h('p', {}, h('b', { text: 'Abandon ship' }), ` stops ${n === 1 ? 'it' : 'them'} and undoes what ${n === 1 ? 'it' : 'they'} did so far: new barrels go overboard (restorable for 30 days) and files copied by a Transfer are deleted. Originals are only ever deleted once a transfer has completed.`),
  ];
  const extra = [];
  if (info.connections?.length) extra.push(`Connections to ${info.connections.join(' and ')} will close.`);
  for (const [name, count] of info.servers || []) extra.push(`${count === 1 ? 'A transfer' : `${count} transfers`} running on ${name} will keep running there.`);
  if (info.paused) extra.push(`${info.paused === 1 ? 'One transfer is' : `${info.paused} transfers are`} already paused and stay${info.paused === 1 ? 's' : ''} paused.`);
  const pauseB = h('button', { class: 'btn primary', text: quitting ? 'Pause and quit' : 'Pause and close' });
  const abandonB = h('button', { class: 'btn', text: quitting ? 'Abandon ship and quit' : 'Abandon ship and close' });
  const keepB = h('button', { class: 'btn', text: quitting ? 'Cancel' : 'Keep open' });
  const m = modal([
    h('div', { class: 'dialog-body' },
      h('h2', { text: quitting ? 'Quit Quartermaster?' : 'Close this window?' }),
      h('p', { text: lines[0] }), list, ...notes, extra.length ? h('div', { class: 'note', text: extra.join(' ') }) : null),
    h('div', { class: 'dialog-foot' }, keepB, h('span', { class: 'spacer' }), abandonB, pauseB),
  ], { onClose: () => { leaving = false; } });
  const choose = (action, button, busy) => async () => {
    for (const b of [pauseB, abandonB, keepB]) b.disabled = true;
    button.textContent = busy;
    try {
      await call(quitting ? 'app_quit' : 'window_close', { action });
    } catch (e) {
      for (const b of [pauseB, abandonB, keepB]) b.disabled = false;
      button.textContent = action === 'pause' ? (quitting ? 'Pause and quit' : 'Pause and close') : (quitting ? 'Abandon ship and quit' : 'Abandon ship and close');
      alertDialog('Couldn’t do that', String(e));
    }
  };
  pauseB.onclick = choose('pause', pauseB, 'Pausing…');
  abandonB.onclick = choose('abandon', abandonB, 'Abandoning…');
  keepB.onclick = () => m.close();
  setTimeout(() => pauseB.focus(), 0);
}

// A help icon whose explanation appears on hover or keyboard focus.
function help(text) {
  let tip = null;
  const hide = () => {
    tip?.remove();
    tip = null;
  };
  const showTip = () => {
    hide();
    tip = h('div', { class: 'tip', role: 'tooltip', text });
    document.body.append(tip);
    const r = icon.getBoundingClientRect();
    const t = tip.getBoundingClientRect();
    const below = r.bottom + 6 + t.height < innerHeight;
    tip.style.left = `${Math.max(8, Math.min(r.left + r.width / 2 - t.width / 2, innerWidth - t.width - 8))}px`;
    tip.style.top = `${below ? r.bottom + 6 : r.top - t.height - 6}px`;
  };
  const icon = h('span', { class: 'help', tabindex: '0', 'aria-label': text, onmouseenter: showTip, onmouseleave: hide, onfocus: showTip, onblur: hide }, I.help());
  return icon;
}

async function infoDialog(i, it) {
  const p = S.panes[i];
  const body = h('div', { class: 'dialog-body' }, h('h2', { text: it.name }), h('div', { class: 'empty' }, h('div', { class: 'spinner' })));
  const m = modal([body, h('div', { class: 'dialog-foot' }, h('button', { class: 'btn primary', text: 'Close', onclick: () => m.close() }))]);
  try {
    const info = await call('archive_info', { serverId: p.loc, path: it.path });
    const saved = info.original_bytes ? Math.round(100 - (info.stored_bytes * 100) / info.original_bytes) : 0;
    const prot = {
      protected: [I.shield, 'ok-text', 'Tally matches the manifest ✓', `Checksums verified and recovery data ready.${info.last_checked ? ` Last checked ${fmtDate(info.last_checked)}.` : ''} No damage found.`],
      pending: [I.shield, 'warn-text', 'Stored and verified', 'Recovery data is being prepared on the server.'],
      damaged: [I.warn, 'bad-text', 'Damaged', 'Some of this data couldn’t be repaired from its recovery data. Contact your archive administrator.'],
      none: [I.info, '', 'Nothing stored', ''],
    }[info.protection];
    body.replaceChildren(
      h('h2', { text: it.name }),
      h('div', { class: 'kv' },
        info.kind === 'dir' ? [h('span', { text: 'Contents' }), h('span', { text: `${plural(info.files, 'file')}, ${plural(info.folders, 'folder')}` })] : null,
        h('span', { text: 'Original size' }), h('span', { text: fmtBytes(info.original_bytes) }),
        h('span', { text: 'Space used' }), h('span', { text: `${fmtBytes(info.stored_bytes)} (${saved}% saved)` }),
        info.duplicate_bytes ? [h('span', { text: 'Duplicates' }), h('span', { text: `${fmtBytes(info.duplicate_bytes)} stored once` })] : null,
        info.archived_at ? [h('span', { text: 'Archived' }), h('span', { text: fmtDate(info.archived_at) })] : null,
        h('span', { text: 'Location' }), h('span', { text: it.path, style: 'user-select:text;-webkit-user-select:text;word-break:break-all' }),
      ),
      h('div', { class: 'status-line' }, h('span', { class: prot[1] }, prot[0]()), h('div', {}, h('div', { class: prot[1], text: prot[2], style: 'font-weight:600' }), h('div', { text: prot[3], style: 'color:var(--text-2)' }))),
    );
  } catch (e) {
    body.replaceChildren(h('h2', { text: it.name }), h('p', { text: String(e) }));
  }
}

async function trashDialog(i) {
  const p = S.panes[i];
  const body = h('div', { class: 'dialog-body' }, h('h2', { text: 'Overboard' }), h('div', { class: 'empty' }, h('div', { class: 'spinner' })));
  const m = modal([body, h('div', { class: 'dialog-foot' }, h('button', { class: 'btn primary', text: 'Close', onclick: () => m.close() }))], { onClose: () => refresh(i) });
  const fill = async () => {
    try {
      const items = await call('archive_trash_list', { serverId: p.loc });
      body.replaceChildren(h('h2', { text: 'Overboard' }), h('p', { text: 'Items stay here for 30 days, then are deleted from the datahold for good and their space is freed.' }));
      if (!items.length) body.append(h('p', { text: 'Nothing has been thrown overboard.' }));
      for (const t of items) {
        const btn = h('button', { class: 'btn', text: 'Restore' });
        btn.onclick = async () => {
          btn.disabled = true;
          try {
            const to = await call('archive_restore', { serverId: p.loc, id: t.id });
            toast(`Restored to ${to}`);
            fill();
          } catch (e) {
            btn.disabled = false;
            alertDialog('Couldn’t restore', String(e));
          }
        };
        body.append(h('div', { class: 'trash-row' },
          h('div', { style: 'min-width:0' }, h('div', { text: t.name }), h('div', { class: 'origin', text: t.origin })),
          h('span', { class: 'num', text: fmtBytes(t.size) }),
          h('span', { class: 'num', text: fmtDate(t.trashed_at) }),
          btn));
      }
    } catch (e) {
      body.replaceChildren(h('h2', { text: 'Overboard' }), h('p', { text: String(e) }));
    }
  };
  fill();
}

function problemsDialog(j) {
  const m = modal([h('div', { class: 'dialog-body' }, h('h2', { text: j.title }), h('p', { text: j.message }), h('div', { class: 'problems', text: j.problems.join('\n') })),
    h('div', { class: 'dialog-foot' }, h('button', { class: 'btn primary', text: 'Close', onclick: () => m.close() }))], { wide: false });
}

// Password, two-factor (2FA), and host-key prompts from ssh. One at a time.
const prompts = [];
let promptOpen = false;
function onPrompt(pr) {
  prompts.push(pr);
  if (!promptOpen) nextPrompt();
}
// Every window is asked a sign-in question; once one answers, the others drop it.
let shownPrompt = null;
function onPromptDone(id) {
  const k = prompts.findIndex((p) => p.id === id);
  if (k >= 0) prompts.splice(k, 1);
  if (shownPrompt?.id === id) shownPrompt.dismiss();
}
function nextPrompt() {
  const pr = prompts.shift();
  if (!pr) { promptOpen = false; return; }
  promptOpen = true;
  let answered = false;
  const reply = (answer) => { answered = true; call('auth_answer', { id: pr.id, answer }); m.close(); };
  let body;
  let foot;
  const who = pr.via || 'this computer';
  if (pr.kind === 'confirm') {
    body = [h('h2', { text: `Is this ${pr.server || 'the server'}?` }), h('p', { text: `This is the first time ${who} has connected to this server. If you weren’t expecting this, choose No and check with whoever runs the server.` }), h('div', { class: 'prompt-text', text: pr.prompt })];
    foot = [h('button', { class: 'btn', text: 'No', onclick: () => reply('no') }), h('button', { class: 'btn primary', text: 'Yes, connect', onclick: () => reply('yes') })];
  } else {
    const input = h('input', { type: pr.kind === 'secret' ? 'password' : 'text', autocomplete: 'off', onkeydown: (e) => e.key === 'Enter' && reply(input.value) });
    const multiline = pr.prompt.includes('\n');
    body = [h('h2', { text: pr.via ? `${pr.via} is signing in to ${pr.server}` : `Sign in to ${pr.server || 'the server'}` }), multiline ? h('div', { class: 'prompt-text', text: pr.prompt }) : null,
      pr.again ? h('p', { class: 'bad-text', text: 'That wasn’t accepted. Try again.' }) : null,
      h('div', { class: 'field' }, h('label', { text: multiline ? 'Your answer' : pr.prompt.replace(/:\s*$/, '') }), input),
      pr.via ? h('div', { class: 'note', text: `${pr.via} keeps this connection open while transfers run, so it can send straight to the datahold. Your ${pr.kind === 'secret' ? 'password' : 'answer'} isn’t saved.` }) : null];
    foot = [h('button', { class: 'btn', text: 'Cancel', onclick: () => reply(null) }), h('button', { class: 'btn primary', text: 'Continue', onclick: () => reply(input.value) })];
    setTimeout(() => input.focus(), 0);
  }
  const m = modal([h('div', { class: 'dialog-body' }, body), h('div', { class: 'dialog-foot' }, foot)], {
    onClose: () => { shownPrompt = null; if (!answered) call('auth_answer', { id: pr.id, answer: null }); setTimeout(nextPrompt, 0); },
  });
  shownPrompt = { id: pr.id, dismiss: () => { answered = true; m.close(); } };
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

function settingsDialog({ add = null } = {}) {
  const draft = structuredClone(S.settings);
  let tab = 'servers';
  let current = add ? null : draft.servers[0]?.id ?? null;
  if (add || !draft.servers.length) {
    const kind = add || (draft.servers.some((x) => x.kind === 'archive') ? 'files' : 'archive');
    const s = { id: '', name: '', kind, host: '', port: null, user: '', root: kind === 'files' ? '~' : '', _new: Math.random() };
    draft.servers.push(s);
    current = s._new;
  }
  const keyOf = (s) => s.id || s._new;
  const body = h('div', { style: 'display:flex;flex-direction:column;flex:1;min-height:0' });
  const m = modal([body, h('div', { class: 'dialog-foot' },
    h('button', { class: 'btn', text: 'Cancel', onclick: () => m.close() }),
    h('button', { class: 'btn primary', text: 'Save', onclick: save }))], { wide: true, onClose: () => resetPanes() });

  async function save() {
    for (const s of draft.servers) {
      if (!s.host.trim()) { alertDialog('Add a server address', `Enter an address (or an ssh alias) for “${s.name || 'the new server'}”, or remove it.`); return; }
      if (s.kind === 'archive' && !s.root.trim()) { alertDialog('Add the datahold location', `Enter the folder on ${s.host} where the datahold lives.`); return; }
      delete s._new;
    }
    try {
      S.settings = await call('settings_save', { settings: draft });
      m.close();
    } catch (e) {
      alertDialog('Couldn’t save settings', String(e));
    }
  }

  function render() {
    body.replaceChildren(h('div', { class: 'tabs' },
      h('button', { class: `tab${tab === 'servers' ? ' on' : ''}`, text: 'Servers', onclick: () => { tab = 'servers'; render(); } }),
      h('button', { class: `tab${tab === 'transfers' ? ' on' : ''}`, text: 'Transfers', onclick: () => { tab = 'transfers'; render(); } }),
      h('button', { class: `tab${tab === 'appearance' ? ' on' : ''}`, text: 'Appearance', onclick: () => { tab = 'appearance'; render(); } })));
    if (tab === 'appearance') {
      // Applies right away and is remembered on this computer, apart from Save and Cancel.
      const current = appearance();
      const choice = (value, label) => h('button', { class: current === value ? 'on' : '', text: label, onclick: () => { setAppearance(value); render(); } });
      body.append(h('div', { class: 'server-form' },
        h('div', { class: 'field' }, h('label', { text: 'Appearance' }),
          h('div', { class: 'seg send-mode' }, choice('system', 'System'), choice('light', 'Light'), choice('dark', 'Dark'))),
        h('p', { class: 'hint', style: 'color:var(--text-3);font-size:12px', text: 'System follows your computer’s light or dark setting.' })));
      return;
    }
    if (tab === 'transfers') {
      const mode = h('select', { onchange: (e) => (draft.default_mode = e.target.value) }, h('option', { value: 'copy', text: 'Copy (keep the originals)' }), h('option', { value: 'move', text: 'Move (delete the originals once everything is archived and verified)' }));
      mode.value = draft.default_mode;
      const hidden = h('input', { type: 'checkbox', checked: draft.show_hidden, onchange: (e) => (draft.show_hidden = e.target.checked), style: 'width:auto;height:auto;margin-right:6px' });
      body.append(h('div', { class: 'server-form' },
        h('div', { class: 'field' }, h('label', { text: 'When sending, start with' }), mode),
        h('div', { class: 'field' }, h('label', { style: 'display:flex;align-items:center;color:var(--text)' }, hidden, 'Show hidden files on this computer')),
        h('p', { class: 'hint', style: 'color:var(--text-3);font-size:12px', text: 'Operating-system files such as .DS_Store, ._ files, Thumbs.db, and desktop.ini are never archived.' })));
      return;
    }
    const list = h('div', { class: 'server-list' });
    for (const s of draft.servers) {
      list.append(h('div', { class: `server-item${keyOf(s) === current ? ' on' : ''}`, onclick: () => { current = keyOf(s); render(); } },
        h('span', { class: `kind-icon${s.kind === 'archive' ? ' pane-icon' : ''}` }, serverIcon(s, s.kind === 'archive' ? 18 : undefined)),
        h('div', { style: 'min-width:0' }, h('div', { text: s.name || s.host || 'New server', style: 'overflow:hidden;text-overflow:ellipsis;white-space:nowrap' }), h('div', { class: 'kind', text: `${s.kind === 'archive' ? 'Datahold' : s.kind === 'sftp' ? 'SFTP server' : 'File server'}${s.temporary ? ' · not saved' : ''}` }))));
    }
    list.append(h('button', { class: 'btn', style: 'margin-top:8px', text: 'Add server', onclick: () => {
      const s = { id: '', name: '', kind: 'files', host: '', port: null, user: '', root: '', _new: Math.random() };
      draft.servers.push(s);
      current = s._new;
      render();
    } }));
    const s = draft.servers.find((x) => keyOf(x) === current);
    const form = h('div', { class: 'server-form' });
    if (!s) form.append(h('p', { style: 'color:var(--text-3)', text: 'Add a server to get started.' }));
    else {
      form.append(
        ...serverFields(s, render, { onRename: () => (list.querySelector('.server-item.on div div').textContent = s.name || s.host || 'New server') }),
        s.temporary ? saveField(s, 'Unticked, it stays here until you quit Quartermaster.') : '',
        s.kind === 'archive' && s.id && (s.allow_keys || (draft.routes || []).some((r) => r.archive === s.id)) ? routesSection(s) : '',
        h('div', { style: 'margin-top:24px' }, h('button', { class: 'link', style: 'color:var(--bad)', text: 'Remove this server', onclick: () => { draft.servers = draft.servers.filter((x) => x !== s); current = draft.servers[0] ? keyOf(draft.servers[0]) : null; render(); } })),
      );
    }
    body.append(h('div', { class: 'settings' }, list, form));
  }

  // File servers with limited keys for this datahold, with a way to remove each.
  function routesSection(s) {
    const routes = (draft.routes || []).filter((r) => r.archive === s.id);
    const box = h('div', { class: 'field', style: 'margin-top:12px' }, h('label', { text: 'File servers with a limited key' }));
    if (!routes.length) {
      box.append(h('div', { class: 'hint', text: 'None yet. A key is set up the first time you transfer between a file server and this datahold.' }));
      return box;
    }
    for (const r of routes) {
      const name = draft.servers.find((x) => x.id === r.files)?.name || r.files;
      const btn = h('button', { class: 'link', style: 'color:var(--bad);margin-left:10px', text: 'Remove' });
      btn.onclick = async () => {
        btn.disabled = true;
        try {
          await call('route_remove', { filesId: r.files, archiveId: r.archive });
          draft.routes = draft.routes.filter((x) => x !== r);
          S.settings.routes = (S.settings.routes || []).filter((x) => !(x.files === r.files && x.archive === r.archive));
          render();
        } catch (e) {
          btn.disabled = false;
          alertDialog('Couldn’t remove it', String(e));
        }
      };
      box.append(h('div', { style: 'display:flex;align-items:center;padding:4px 0' }, h('span', { text: name }), btn));
    }
    return box;
  }

  render();
}

// The fields describing a server, shared by Settings and the Add dialog. `kinds` limits the
// types offered (the Type menu is left out when there's only one).
function serverFields(s, render, { kinds = ['archive', 'files', 'sftp'], onRename = () => {} } = {}) {
  const input = (key, attrs = {}) => h('input', { value: s[key] ?? '', ...attrs, oninput: (e) => { s[key] = key === 'port' ? (Number(e.target.value) || null) : e.target.value; if (key === 'name' || key === 'host') onRename(); } });
  const labels = { archive: 'Datahold (shown on the right)', files: 'File server, such as an analysis server (shown on the left)', sftp: 'SFTP server, without the helper (Transfer mode only)' };
  const kind = h('select', { onchange: (e) => { s.kind = e.target.value; render(); } }, kinds.map((k) => h('option', { value: k, text: labels[k] })));
  kind.value = s.kind;
  const result = h('div', { class: 'status-line' });
  const testBtn = h('button', { class: 'btn', text: 'Test connection' });
  testBtn.onclick = async () => {
    if (!s.host.trim()) { result.replaceChildren(h('span', { class: 'bad-text', text: 'Enter the server address first.' })); return; }
    testBtn.disabled = true;
    result.replaceChildren(h('div', { class: 'spinner', style: 'width:14px;height:14px' }), h('span', { text: 'Connecting… (answer any password or two-factor prompt)' }));
    const info = await call('test_server', { server: { ...s, id: s.id || 'new' } }).catch((e) => ({ connected: false, message: String(e) }));
    testBtn.disabled = false;
    showTest(s, info, result, testBtn);
  };
  return [
    h('div', { class: 'field' }, h('label', { text: 'Display name' }), input('name', { placeholder: s.kind === 'archive' ? 'Lab datahold' : 'Analysis server' })),
    kinds.length > 1 ? h('div', { class: 'field' }, h('label', { text: 'Type' }), kind) : '',
    h('div', { class: 'grid2' },
      h('div', { class: 'field' }, h('label', { text: 'Server address' }), input('host', { placeholder: 'server.example.edu', autocapitalize: 'off', spellcheck: 'false' }), h('div', { class: 'hint', text: 'A host name, or a name from your ~/.ssh/config.' })),
      h('div', { class: 'field' }, h('label', { text: 'Port' }), input('port', { placeholder: '22', inputmode: 'numeric' }))),
    h('div', { class: 'field' }, h('label', { text: 'Username' }), input('user', { placeholder: 'Leave blank to use your SSH settings', autocapitalize: 'off', spellcheck: 'false' }), h('div', { class: 'hint', text: 'Your SSH key is used if you have one; otherwise the app asks for your password (and a two-factor code, if the server uses one).' })),
    h('div', { class: 'field' }, h('label', { text: s.kind === 'archive' ? 'Datahold location on the server' : 'Start in folder' }), input('root', { placeholder: s.kind === 'archive' ? '/mnt/pool/lab/archive' : '~', autocapitalize: 'off', spellcheck: 'false' })),
    h('div', { style: 'display:flex;align-items:center;gap:10px;margin-top:4px' }, testBtn),
    result,
    s.kind === 'archive' ? keysField(s, render) : s.kind === 'sftp' ? readBackField(s, render) : relayField(s, render),
  ];
}

// Whether a server is kept in Settings, or only until the app quits.
function saveField(s, hint) {
  const box = h('input', { type: 'checkbox', checked: !s.temporary, onchange: (e) => (s.temporary = !e.target.checked) });
  return h('div', { class: 'field', style: 'margin-top:18px' },
    h('label', { class: 'check' }, box, h('span', { text: 'Save this server' })),
    h('div', { class: 'hint', text: hint }));
}

// Add a server from a pane's location menu: just the server's details, then connect. It's saved
// in Settings unless "Save this server" is unticked, which makes it a one-off connection.
function addServerDialog(i, kinds) {
  const s = { id: `s${Math.random().toString(16).slice(2, 14)}`, name: '', kind: kinds[0], host: '', port: null, user: '', root: kinds[0] === 'archive' ? '' : '~', temporary: false };
  const body = h('div', { class: 'server-form', style: 'padding:0' });
  const title = h('h2', {});
  const m = modal([h('div', { class: 'dialog-body' }, title, body), h('div', { class: 'dialog-foot' },
    h('button', { class: 'btn', text: 'Cancel', onclick: () => m.close() }),
    h('button', { class: 'btn primary', text: 'Connect', onclick: connect }))], { onClose: () => renderPane(i) });
  function render() {
    title.textContent = s.kind === 'archive' ? 'Add a datahold' : s.kind === 'sftp' ? 'Add an SFTP server' : 'Add a file server';
    body.replaceChildren(...serverFields(s, render, { kinds }),
      saveField(s, 'Unticked, it’s a one-off connection: it stays in the menu until you quit Quartermaster.'));
  }
  async function connect() {
    if (!s.host.trim()) return alertDialog('Add a server address', 'Enter an address, or a name from your ~/.ssh/config.');
    if (s.kind === 'archive' && !s.root.trim()) return alertDialog('Add the datahold location', `Enter the folder on ${s.host} where the datahold lives.`);
    try {
      S.settings = await call('settings_save', { settings: { ...S.settings, servers: [...S.settings.servers, s] } });
    } catch (e) {
      return alertDialog('Couldn’t add the server', String(e));
    }
    m.close();
    openLocation(i, s.id);
  }
  render();
}

// An SFTP server can't compute checksums, so copies to it are checked by size, unless uploads
// are read back and compared.
function readBackField(s, render) {
  const box = h('input', { type: 'checkbox', checked: !!s.read_back, onchange: (e) => { s.read_back = e.target.checked; render(); } });
  return h('div', { class: 'field', style: 'margin-top:18px' },
    h('label', { class: 'check' }, box, h('span', { text: 'Check uploads by reading them back' })),
    h('div', { class: 'hint', text: s.read_back
      ? 'Each file copied to this server is read back and its checksum compared, as thoroughly as with the helper. Uploads take about twice as long.'
      : 'An SFTP server can’t compute checksums, so copies are checked by size and date, and the Dock says “Size matches”. Turn this on for data that matters.' }));
}

// Whether a file server's transfers go straight to archives or through this computer.
function relayField(s, render) {
  const box = h('input', { type: 'checkbox', checked: !!s.relay, onchange: (e) => { s.relay = e.target.checked; render(); } });
  return h('div', { class: 'field', style: 'margin-top:18px' },
    h('label', { class: 'check' }, box, h('span', { text: 'Send through this computer' })),
    h('div', { class: 'hint', text: s.relay
      ? 'Transfers between this server and dataholds pass through this computer, which must stay awake with the app open until they finish. Use this if the server can’t reach the datahold, or doesn’t let programs keep running after you log out.'
      : 'Transfers run on this server and go straight to the datahold, so you can close the app while they run.' }));
}

// How file servers reach this archive: by signing in (the default) or with a limited key.
function keysField(s, render) {
  const box = h('input', { type: 'checkbox', checked: !!s.allow_keys, onchange: (e) => { s.allow_keys = e.target.checked; render(); } });
  return h('div', { class: 'field', style: 'margin-top:18px' },
    h('label', { class: 'check' }, box, h('span', { text: 'Let file servers use a limited key for this datahold' })),
    h('div', { class: 'hint', text: s.allow_keys
      ? 'A file server gets a key that can only add and read data here, so its transfers never need a sign-in. This works only if the datahold’s server accepts SSH keys.'
      : 'File servers sign in with your password (and two-factor code, if used) when a transfer starts, and keep that connection open while transfers run.' }));
}

function showTest(s, info, result, testBtn) {
  result.replaceChildren();
  if (!info.connected) {
    result.append(h('span', { class: 'bad-text', text: `✕ ${info.message || 'Couldn’t connect.'}` }));
    return;
  }
  const parts = ['Connected', info.os, info.helper, info.free_bytes != null ? `${fmtBytes(info.free_bytes)} free` : null].filter(Boolean);
  if (!info.needs) {
    result.append(h('span', { class: 'ok-text', text: `✓ ${parts.join(' · ')}` }));
    return;
  }
  if (info.needs === 'helper' || info.needs === 'update') {
    const btn = h('button', { class: 'btn', text: info.needs === 'update' ? 'Update helper' : 'Install helper' });
    btn.onclick = async () => {
      btn.disabled = true;
      btn.textContent = 'Installing…';
      try {
        await call('install_helper', { server: s });
        testBtn.click();
      } catch (e) {
        btn.disabled = false;
        btn.textContent = 'Install helper';
        result.append(h('div', { class: 'bad-text', text: String(e) }));
      }
    };
    result.append(h('div', {}, h('div', { class: s.kind === 'archive' ? 'warn-text' : 'ok-text', text: `${s.kind === 'archive' ? '!' : '✓'} ${parts.join(' · ')}` }),
      h('div', { style: 'margin:6px 0', text: info.needs === 'update' ? 'The Quartermaster helper on this server is out of date for this app.' : s.kind === 'archive' ? 'The Quartermaster helper isn’t installed on this server yet.' : 'The Quartermaster helper isn’t installed here yet. It will be needed for direct server-to-server transfers.' }), btn));
    return;
  }
  if (info.needs === 'archive') {
    const btn = h('button', { class: 'btn', text: 'Create a datahold here' });
    btn.onclick = async () => {
      btn.disabled = true;
      try {
        await call('create_archive', { server: s });
        testBtn.click();
      } catch (e) {
        btn.disabled = false;
        result.append(h('div', { class: 'bad-text', text: String(e) }));
      }
    };
    result.append(h('div', {}, h('div', { class: 'ok-text', text: `✓ ${parts.join(' · ')}` }), h('div', { style: 'margin:6px 0', text: `There’s no datahold at ${s.root} yet.` }), btn));
  }
}

// Which locations each pane may show in each mode.
function validLoc(mode, i, loc) {
  if (mode === 'stow' && i === 1) return serverById(loc)?.kind === 'archive';
  if (mode === 'stow') return loc === 'local' || serverById(loc)?.kind === 'files';
  return loc === 'local' || ['files', 'sftp'].includes(serverById(loc)?.kind);
}
function defaultLoc(mode, i) {
  const first = (kind) => S.settings.servers.find((x) => x.kind === kind)?.id ?? null;
  if (mode === 'stow') return i === 0 ? 'local' : first('archive');
  return i === 0 ? 'local' : first('files') ?? first('sftp');
}
// Keep each pane where it is if that's still allowed, and open it.
function resetPanes() {
  S.panes.forEach((p, i) => openLocation(i, p.loc && validLoc(S.mode, i, p.loc) ? p.loc : defaultLoc(S.mode, i)));
  S.opened.add(S.mode);
}

// The switch in the top bar, and the line under it that says what this mode does.
function renderMode() {
  document.body.dataset.mode = S.mode;
  const label = { stow: 'Stow', transfer: 'Transfer' };
  const tip = {
    stow: 'Stow: put folders into the datahold as sealed barrels, checked, compressed, and protected',
    transfer: 'Transfer: copy files exactly as they are between this computer and file servers',
  };
  document.getElementById('mode-switch').replaceChildren(...['stow', 'transfer'].map((m) =>
    h('button', { class: S.mode === m ? 'on' : '', role: 'tab', 'aria-selected': String(S.mode === m), title: tip[m], text: label[m], onclick: () => switchMode(m) })));
  document.getElementById('modebar').replaceChildren(
    h('span', { class: 'ic' }, stowMode() ? I.barrel() : I.swap()),
    h('span', { text: stowMode()
      ? 'Stowing: folders go into the datahold as sealed barrels, checked, compressed, and protected.'
      : 'Transferring: files are copied exactly as they are between this computer and file servers. Nothing is compressed or sealed.' }));
}

function switchMode(mode) {
  if (mode === S.mode) return;
  document.querySelector('.menu')?.remove();
  S.mode = mode;
  S.panes = S.modes[mode];
  try {
    localStorage.setItem('mode', mode);
  } catch {}
  renderMode();
  // Servers may have been added or removed since this mode's panes were last shown.
  const fine = S.opened.has(mode) && S.panes.every((p, i) => (p.loc ? validLoc(mode, i, p.loc) : defaultLoc(mode, i) == null));
  if (!fine) resetPanes();
  else S.panes.forEach((p, i) => (p.status === 'ready' && !p.hits ? load(i, p.path, { push: false }) : renderPane(i)));
  updateXferButtons();
}

// ---------------------------------------------------------------------------
// Start
// ---------------------------------------------------------------------------

async function start() {
  // In the Mac app the window buttons sit in the top bar.
  if (isApp && /Mac/.test(navigator.userAgent)) document.body.classList.add('mac-titlebar');
  document.getElementById('brand').append(h('span', { class: 'brand-mark' }, I.wheel()), 'Quartermaster');
  document.getElementById('settings-btn').append(I.gear());
  document.getElementById('settings-btn').onclick = () => settingsDialog();
  document.getElementById('to-right').append(I.right());
  document.getElementById('to-left').append(I.left());
  document.querySelector('.xfer-ship').append(I.frigate());
  document.getElementById('to-right').onclick = (e) => !e.currentTarget.classList.contains('off') && transfer(0, 1);
  document.getElementById('to-left').onclick = (e) => !e.currentTarget.classList.contains('off') && transfer(1, 0);
  new ResizeObserver(fitXfer).observe(document.querySelector('.main'));
  renderMode();

  document.addEventListener('keydown', (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'l' && !document.querySelector('.overlay')) {
      e.preventDefault();
      editPath(S.focus ?? 0);
    }
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'f' && !document.querySelector('.overlay')) {
      e.preventDefault();
      const i = S.focus ?? 0;
      if (!S.panes[i].searchOpen) toggleSearch(i);
      else document.querySelector(`#pane${i} .search-row input`)?.focus();
    }
  });
  on('close-requested', (info) => leaveDialog(info, false));
  on('quit-requested', (info) => leaveDialog(info, true));
  on('auth-prompt', onPrompt);
  on('auth-prompt-done', onPromptDone);
  // Servers added or removed in another window.
  on('settings-changed', (settings) => {
    S.settings = settings;
    if (S.panes.some((p, i) => p.loc && !validLoc(S.mode, i, p.loc))) resetPanes();
    else S.panes.forEach((_, i) => renderPane(i));
  });
  on('transfer', onTransfer);
  onDragDrop((type, paths, position) => {
    // Drags between the panes arrive here too (see paneDrop).
    if (S.dragging != null || Date.now() - (S.dragEnded || 0) < 1500) return type === 'drop' && paneDrop(position);
    return stowMode() ? stowDrop(type, paths, position) : transferDrop(type, paths, position);
  });

  S.settings = await call('settings_get');
  updateXferButtons();
  for (const j of await call('transfers')) S.jobs.set(j.id, j);
  renderTransfers();
  // Refresh the Dock every second while something is moving, so times and
  // "still working" notes stay current between progress reports.
  setInterval(() => {
    if ([...S.jobs.values()].some((j) => ['running', 'waiting'].includes(j.state))) renderTransfers();
  }, 1000);
  resetPanes();
  if (stowMode() && !S.settings.servers.some((s) => s.kind === 'archive')) settingsDialog({ add: 'archive' });
  if (!isApp) document.title = 'Quartermaster (preview with sample data)';
}

start();
