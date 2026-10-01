// Calls into the app's backend. Opened in a plain browser (for design work),
// it falls back to a small mock with sample data.

const T = window.__TAURI__;
export const isApp = Boolean(T && T.core);

export function call(cmd, args = {}) {
  return isApp ? T.core.invoke(cmd, args) : mock(cmd, args);
}

export function on(event, cb) {
  // Listen as this window, not globally: an event sent to one window (a sign-in question, say)
  // reaches a global listener in every window, but a window's own listener only when it is
  // that window's or sent to all.
  if (isApp) return (T.webviewWindow ? T.webviewWindow.getCurrentWebviewWindow() : T.event).listen(event, (e) => cb(e.payload));
  (listeners[event] ||= []).push(cb);
  return Promise.resolve(() => {});
}

// Drags onto the window, in the app: type is 'enter', 'over', 'drop', or 'leave'.
// Paths come with 'enter' and 'drop'; the position is in physical pixels.
export function onDragDrop(cb) {
  if (!isApp || !T.webview) return;
  T.webview.getCurrentWebview().onDragDropEvent((e) => cb(e.payload.type, e.payload.paths, e.payload.position));
}

// ---------------------------------------------------------------------------
// Mock backend for browser previews
// ---------------------------------------------------------------------------

const listeners = {};
const emit = (ev, p) => (listeners[ev] || []).forEach((cb) => cb(p));
// For design work in a plain browser: window.mockEmit('close-requested', { transfers: [...] }).
if (!isApp) window.mockEmit = emit;
const now = Math.floor(Date.now() / 1000);
const day = 86400;

const mockSettings = {
  servers: [
    { id: 'hill', name: 'Lab datahold', kind: 'archive', host: 'lab-storage', port: null, user: '', root: '/mnt/pool/lab/archive' },
    { id: 'compute', name: 'lab-compute', kind: 'files', host: 'lab-compute', port: null, user: '', root: '/home/alex' },
    { id: 'rc', name: 'Campus cluster', kind: 'files', host: 'cluster.example.edu', port: null, user: 'alex', root: '~' },
    { id: 'partner', name: 'partner', kind: 'sftp', host: 'partner.example.edu', port: null, user: 'alex', root: '~', read_back: false },
  ],
  default_mode: 'copy',
  show_hidden: false,
};

const localTree = {
  '/Users/jhill': [['Research', 'dir'], ['Desktop', 'dir'], ['Documents', 'dir'], ['notes.txt', 'file', 4200]],
  '/Users/jhill/Research': [['2024_sequencing', 'dir'], ['locked_data', 'dir'], ['grant_2025.pdf', 'file', 2_100_000]],
  '/Users/jhill/Research/2024_sequencing': [
    ['raw_reads', 'dir'], ['aligned', 'dir'], ['analysis', 'dir'],
    ['sample_sheet.csv', 'file', 12_000], ['run_summary.pdf', 'file', 2_100_000],
  ],
};

const archiveTree = {
  '/': [['Projects', 'dir', 3_456_000_000_000, 51_800]],
  '/Projects': [
    ['Charity_Dairy', 'dir', 1_900_000_000_000, 41_206], ['Caroline_Oswald', 'dir', 840_000_000_000, 6_100],
    ['Kaitlyn_Robinson', 'dir', 620_000_000_000, 4_390], ['methylation_pilot', 'dir', 96_000_000_000, 104],
    ['README.txt', 'file', 4_000, 1],
  ],
  '/Projects/Charity_Dairy': [
    ['raw_reads', 'dir', 1_600_000_000_000, 38_000], ['analysis', 'dir', 290_000_000_000, 3_200],
    ['sample_sheet.csv', 'file', 14_000, 1],
  ],
};

// Sample transfers move along on a timer until they finish, pause, or stop.
const timers = {};
function tick(job) {
  clearInterval(timers[job.id]);
  timers[job.id] = setInterval(() => {
    job.done = Math.min(job.total, job.done + job.total / 60);
    if (job.done >= job.total) {
      job.state = 'done';
      job.message = job.direction === 'copy' ? '2,140 of 2,140 files copied and verified' : '18,412 of 18,412 files archived and verified; 18,412 removed from this computer (240 GB sent)';
      clearInterval(timers[job.id]);
    }
    emit('transfer', { ...job });
  }, 300);
}

function mockControl(id, what) {
  const job = jobs.find((j) => j.id === id);
  if (!job) return null;
  clearInterval(timers[id]);
  if (what === 'pause') Object.assign(job, { state: 'paused', rate: 0, message: 'Paused. Press play to continue where it stopped; files already archived are kept.' });
  if (what === 'cancel') Object.assign(job, { state: 'cancelled', rate: 0, message: 'Stopped. Files already archived stay archived.' });
  if (what === 'resume') {
    Object.assign(job, { state: 'running', rate: 38_000_000, message: 'Transferring', link: null });
    tick(job);
  }
  emit('transfer', { ...job });
  return null;
}

function mockList(tree, path, sep, archive) {
  // Like the real listings: ~ is the home folder, a trailing / is ignored, and unknown folders fail.
  path = path.replace(/^~(?=\/|$)/, '/Users/jhill');
  const segs = [];
  for (const seg of path.split('/')) seg === '..' ? segs.pop() : seg && seg !== '.' && segs.push(seg);
  path = '/' + segs.join('/');
  if (!tree[path] && !(archive && path.startsWith('/Projects'))) throw `Can’t open ${path}: No such file or directory`;
  const rows = tree[path] || [];
  const items = rows.map(([name, kind, size, files], i) => ({
    name,
    path: (path === '/' ? '' : path) + sep + name,
    kind,
    size: kind === 'dir' ? (archive ? size : null) : size,
    files: archive && kind === 'dir' ? files : null,
    mtime: now - (i + 3) * 40 * day,
    archived: archive ? now - (i + 1) * 5 * day : null,
    // In the sample, each folder in /Projects is an archived project.
    is_project: archive && kind === 'dir' && path === '/Projects',
    in_project: archive && path.startsWith('/Projects/'),
  }));
  const parts = path.split('/').filter(Boolean);
  const crumbs = archive
    ? [{ name: 'Lab datahold', path: '/' }, ...parts.map((p, i) => ({ name: p, path: '/' + parts.slice(0, i + 1).join('/') }))]
    : [{ name: 'Home', path: '/Users/jhill' }, ...parts.slice(2).map((p, i) => ({ name: p, path: '/' + parts.slice(0, i + 3).join('/') }))];
  const parent = path === '/' ? null : path.replace(/\/[^/]*$/, '') || '/';
  const project = archive && parts[0] === 'Projects' && parts.length >= 2 ? `/Projects/${parts[1]}` : null;
  return { path, parent, crumbs, items, project, free_bytes: archive ? 7_200_000_000_000 : 412_000_000_000 };
}

let jobId = 1;
// A server job paused because its sign-in closed, to preview that row.
const jobs = [{ id: 'R:hc:1', title: 'Moving run_demo → Projects (on lab-compute)', direction: 'send', state: 'waiting', done: 88_000_000, total: 210_000_000, current: '', message: 'Paused: the connection to storage.example.edu closed. Sign in again in the Archive app and this transfer continues where it stopped.', problems: [], rate: 0, server: 'lab-compute', archive: 'Lab datahold', link: { host: 'storage.example.edu', port: null, user: 'alex' } }];
let promptId = 100;
const answers = {};
// Show a sign-in question as the real backend would, and wait for the answer.
function mockAsk(server, via, prompt) {
  const id = promptId++;
  return new Promise((resolve) => {
    answers[id] = resolve;
    emit('auth-prompt', { id, server, via, prompt, kind: 'secret' });
  });
}

async function mock(cmd, a) {
  await new Promise((r) => setTimeout(r, 120));
  switch (cmd) {
    case 'settings_get': return structuredClone(mockSettings);
    case 'settings_save': Object.assign(mockSettings, a.settings); return mockSettings;
    case 'places_server': return [{ name: 'Home', path: '/home/alex' }, { name: 'storage', path: '/media/storage' }];
    case 'places': return [{ name: 'Home', path: '/Users/jhill' }, { name: 'Desktop', path: '/Users/jhill/Desktop' }, { name: 'Box', path: '/Users/jhill/Library/CloudStorage/Box-Box' }, { name: 'BigData', path: '/Volumes/BigData' }];
    case 'list_local': return mockList(localTree, a.path || '/Users/jhill', '/', false);
    case 'list_archive': return mockList(archiveTree, a.path || '/', '/', true);
    case 'connect':
      if (a.serverId === 'rc') {
        setTimeout(() => emit('auth-prompt', { id: 1, server: 'Campus cluster', kind: 'text', prompt: 'Duo two-factor login for alex\n\nEnter a passcode or select one of the following options:\n\n 1. Duo Push to XXX-XXX-1234\n 2. Phone call to XXX-XXX-1234\n\nPasscode or option (1-2):' }), 200);
        return { kind: 'files', connected: true, needs: null, helper: 'archive-helper 0.1.0', os: 'linux', host: 'login1.cluster.example.edu' };
      }
      if (a.serverId === 'compute') return { kind: 'files', connected: true, needs: null, os: 'linux', helper: 'archive-helper 0.1.0', host: 'lab-compute-1' };
      return { kind: 'archive', connected: true, needs: null, helper: 'archive-helper 0.1.0', os: 'freebsd', free_bytes: 7_200_000_000_000 };
    case 'test_server': return { kind: a.server.kind, connected: true, needs: null, helper: 'archive-helper 0.1.0 (protocol 1, freebsd-x86_64)', os: 'freebsd', free_bytes: 7_200_000_000_000 };
    case 'archive_info':
      return { path: a.path, kind: 'dir', files: 41206, folders: 318, original_bytes: 1_900_000_000_000, stored_bytes: 1_210_000_000_000, duplicate_bytes: 212_000_000_000, archived_at: now - 7 * day, protection: 'protected', last_checked: now - 23 * day };
    case 'archive_search': return [{ folder: '/Projects/Charity_Dairy', item: { name: 'sample_' + a.query + '.fastq.gz', path: '/Projects/Charity_Dairy/sample.fastq.gz', kind: 'file', size: 3_200_000_000, mtime: now - 400 * day, archived: now - 7 * day } }];
    case 'search_local':
      return {
        hits: [
          { folder: a.root + '/raw_reads', item: { name: `run1_${a.query}.fastq.gz`, path: a.root + `/raw_reads/run1_${a.query}.fastq.gz`, kind: 'file', size: 4_100_000_000, mtime: now - 90 * day } },
          { folder: a.root + '/analysis/tables', item: { name: `${a.query}_summary.csv`, path: a.root + `/analysis/tables/${a.query}_summary.csv`, kind: 'file', size: 81_000, mtime: now - 20 * day } },
          { folder: a.root, item: { name: `${a.query}_plots`, path: a.root + `/${a.query}_plots`, kind: 'dir', size: null, mtime: now - 12 * day } },
        ],
        truncated: false,
        timed_out: false,
        scanned: a.everyFile ? 18_412 : 0,
        method: a.everyFile ? 'scan' : 'spotlight',
      };
    case 'search_cancel': return null;
    case 'list_server': {
      const l = mockList(localTree, a.path || '/Users/jhill/Research', '/', false);
      return { ...l, crumbs: l.crumbs.map((c) => (c.name === 'Home' ? { ...c, name: 'alex' } : c)) };
    }
    case 'search_server': return mock('search_local', a);
    case 'measure_server': return { files: 12000, folders: 20, bytes: 3_100_000_000_000 };
    case 'mkdir_server': return null;
    case 'trash_local': return null;
    case 'delete_server': return a.paths.length;
    case 'trash_server': {
      // For previewing: a name with "locked" in it is one the server can't make a Trash for.
      const stuck = a.paths.filter((p) => p.includes('locked'));
      return {
        trashed: a.paths.filter((p) => !stuck.includes(p)).map((p, k) => ({ id: `t${k}`, name: p.split('/').pop(), original: p, stored: `/home/alex/.local/share/archive-helper/trash/items/t${k}/${p.split('/').pop()}`, kind: 'dir', size: 0, trashed_at: now })),
        failed: stuck.map((p) => ({ path: p, reason: 'a Trash can’t be made on that disk (Permission denied)', can_delete: true })),
      };
    }
    case 'trash_list_server': return [
      { id: 'a1', name: 'old_run', original: '/home/alex/Research/old_run', stored: '/home/alex/.local/share/archive-helper/trash/items/a1/old_run', kind: 'dir', size: 0, trashed_at: now - 2 * day },
      { id: 'a2', name: 'scratch.fastq', original: '/home/alex/Research/scratch.fastq', stored: '/home/alex/.local/share/archive-helper/trash/items/a2/scratch.fastq', kind: 'file', size: 41_000_000_000, trashed_at: now - 9 * day },
      { id: 'a3', name: 'notes_old.txt', original: '/home/alex/notes_old.txt', stored: '/home/alex/.local/share/archive-helper/trash/items/a3/notes_old.txt', kind: 'file', size: 3_400, trashed_at: now - 20 * day },
    ];
    case 'trash_restore_server': return '/home/alex/Research/old_run';
    case 'trash_empty_server': return a.ids ? a.ids.length : 3;
    case 'route_status': return { same_machine: false, ready: mockSettings.routes?.length > 0, keys: false };
    case 'route_setup': (mockSettings.routes ||= []).push({ files: a.filesId, archive: a.archiveId }); return null;
    case 'route_remove': return null;
    case 'archive_trash_list': return [{ id: 7, name: 'old_run', origin: '/Projects/old_run', kind: 'dir', size: 12_000_000_000, files: 210, trashed_at: now - 3 * day }];
    case 'archive_restore': return '/Projects/old_run';
    case 'measure_local': return { files: 18412, folders: 36, bytes: 600_000_000_000 };
    case 'transfers': return jobs;
    case 'auth_answer': answers[a.id]?.(a.answer); delete answers[a.id]; return null;
    case 'window_close':
    case 'app_quit': return null;
    case 'job_sign_in': {
      const answer = await mockAsk('Lab datahold', 'lab-compute', 'alex@storage.example.edu’s password:');
      if (answer == null) throw 'The sign-in to storage.example.edu was cancelled.';
      const job = jobs.find((j) => j.id === a.id);
      Object.assign(job, { state: 'running', message: 'Transferring', rate: 38_000_000 });
      emit('transfer', { ...job });
      return null;
    }
    case 'transfers_clear':
      jobs.splice(0, jobs.length, ...jobs.filter((j) => !['done', 'failed', 'cancelled', 'interrupted'].includes(j.state)));
      return null;
    case 'transfer_dismiss': {
      const k = jobs.findIndex((j) => j.id === a.id);
      if (k >= 0) jobs.splice(k, 1);
      return null;
    }
    case 'transfer_pause': return mockControl(a.id, 'pause');
    case 'transfer_resume': return mockControl(a.id, 'resume');
    case 'transfer_cancel': return mockControl(a.id, 'cancel');
    case 'transfer_abandon': {
      mockControl(a.id, 'cancel');
      const job = jobs.find((j) => j.id === a.id);
      if (job) {
        Object.assign(job, { message: `Abandoned: ${(job.created || []).map((p) => `“${p.split('/').pop()}”`).join(', ')} went overboard, where it can be brought back for 30 days. Nothing was removed from where it came from.`, created: [] });
        emit('transfer', { ...job });
      }
      return null;
    }
    case 'transfers_pause_all':
      for (const j of jobs.filter((j) => (a.pause ? ['queued', 'running', 'waiting'] : ['paused']).includes(j.state))) mockControl(j.id, a.pause ? 'pause' : 'resume');
      return null;
    case 'transfer_busy': {
      // As in the app: the same items (or a folder around them) already in a transfer in progress.
      const inside = (x, y) => x === y || x.startsWith(y.replace(/\/$/, '') + '/');
      const busy = jobs.find((j) => ['queued', 'running', 'waiting', 'paused'].includes(j.state) && j.place === (a.req.files_id || '')
        && a.req.sources.some((s) => (j.sources || []).some((t) => inside(s, t) || inside(t, s))));
      return busy ? `“${a.req.sources[0].split('/').pop()}” is already being transferred. Wait for that transfer to finish, or stop it first.` : null;
    }
    case 'transfer_start': {
      if (a.req.files_id === 'rc' && !a.req.relay) throw 'RELAY_OFFER|Campus cluster can’t reach storage.example.edu over the network (Connection timed out).';
      if (a.req.files_id && !a.req.relay) {
        const answer = await mockAsk('Lab datahold', 'lab-compute', 'alex@storage.example.edu’s password:');
        if (answer == null) throw 'The sign-in to storage.example.edu was cancelled.';
      }
      const id = jobId++;
      if (a.req.direction === 'copy') {
        const name = a.req.sources[0].split('/').pop();
        const [from, to] = [a.req.files_id && mockSettings.servers.find((x) => x.id === a.req.files_id).name, a.req.to_id && mockSettings.servers.find((x) => x.id === a.req.to_id).name];
        const copyJob = { id: String(id), title: `${a.req.mode === 'move' ? 'Moving' : 'Copying'} ${name}${from ? ` from ${from}` : ''} → ${to || a.req.dest.split('/').pop()}`, direction: 'copy', state: 'running', done: 0, total: 3_100_000_000, current: 'reads_001.fastq.gz', message: 'Transferring', problems: [], rate: 62_000_000, server: null, relay: false, sources: a.req.sources, place: a.req.files_id || '', created: a.req.mode === 'move' ? [] : [`${a.req.dest.replace(/\/$/, '')}/${name}`] };
        jobs.push(copyJob);
        tick(copyJob);
        emit('transfer', { ...copyJob });
        return String(id);
      }
      const job = { id: String(id), title: 'Moving raw_reads → Projects', direction: a.req.direction, state: 'running', done: 0, total: 412_000_000_000, current: 'reads_001.fastq.gz', message: 'Transferring', problems: [], rate: 38_000_000, server: a.req.files_id ? (a.req.files_id === 'rc' ? 'Campus cluster' : 'lab-compute') : null, relay: !!a.req.relay, sources: a.req.sources, place: a.req.files_id || '', created: a.req.direction === 'send' ? a.req.sources.map((s) => `${a.req.dest.replace(/\/$/, '')}/${s.split('/').pop()}`) : [] };
      jobs.push(job);
      tick(job);
      emit('transfer', { ...job });
      return id;
    }
    default: return null;
  }
}
