//! The transfer queue, reported to the window as `transfer` events.
//!
//! Transfers from this computer run here, one at a time, each on its own
//! (shared-login) connection. Transfers from a file server run on that server
//! as jobs (see `archive_core::jobs`); the app only watches them, polling each
//! server that has jobs in progress every second, and shows jobs it finds
//! when it connects. A server job whose signed-in connection to the archive
//! closed waits, and its row offers to sign in again.
//!
//! A transfer from this computer that loses its connection is paused, with a
//! note saying so, and continues where it stopped when it's played again (which
//! reconnects, asking for a password if needed). Nothing retries by itself, so
//! a long outage never looks like an attack on the server.
//!
//! Any unfinished transfer can be paused: it stops at the next file and is set
//! aside, so the queue moves on, until it's played again. Unfinished transfers
//! from this computer are saved to disk, so after the app quits they come back
//! paused and continue where they stopped.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use archive_core::remote::Remote;
use archive_core::retrieve::{self, LocalConflict, RetrieveOptions};
use archive_core::send::{self, Mode, SendOptions};
use archive_core::util::human_bytes;
use archive_core::{Conflict, VPath};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

/// Sends one window its transfer updates, as `transfer` events. Every window has its own
/// queue, so a window only ever hears about its own transfers (and the server jobs it shows).
#[derive(Clone)]
pub struct Notifier {
    app: AppHandle,
    label: String,
}

impl Notifier {
    pub fn new(app: AppHandle, label: &str) -> Notifier {
        Notifier { app, label: label.to_string() }
    }

    fn emit(&self, event: &str, payload: JobView) -> tauri::Result<()> {
        self.app.emit_to(self.label.as_str(), event, payload)
    }
}

use archive_core::jobs::JobStatus;
use archive_core::link::LinkTarget;

use crate::conn::{Conns, Ssh};
use crate::settings::Server;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferRequest {
    /// "send" (to the archive), "retrieve" (from the archive), or "copy"
    /// (plain files between this computer and file servers, in Transfer mode).
    pub direction: String,
    /// The archive (not used by "copy").
    pub server_id: String,
    /// The file server the left pane shows, or None for this computer. For a
    /// "copy": the server the items come from.
    #[serde(default)]
    pub files_id: Option<String>,
    /// A "copy" only: the file server the items go to, or None for this computer.
    #[serde(default)]
    pub to_id: Option<String>,
    pub sources: Vec<String>,
    pub dest: String,
    /// "copy" or "move" (retrieval is always a copy).
    pub mode: String,
    /// "skip" or "keep-both" when sending; "skip", "replace", or "keep-both" when retrieving.
    pub conflict: String,
    /// Sending: put everything in a new project folder of this name in `dest`.
    #[serde(default)]
    pub new_project: Option<String>,
    /// Send through this computer instead of from the file server directly.
    #[serde(default)]
    pub relay: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct JobView {
    /// "L<n>" for transfers from this computer, "R:<server>:<job>" for server jobs.
    pub id: String,
    pub title: String,
    pub direction: String,
    /// "queued", "running", "waiting" (to reconnect or sign in again),
    /// "paused", "done", "failed", "cancelled", or "interrupted".
    pub state: String,
    pub done: u64,
    pub total: u64,
    pub current: String,
    pub message: String,
    pub problems: Vec<String>,
    pub started: Option<u64>,
    /// Bytes per second over the transfer so far.
    pub rate: u64,
    /// The server a job runs on (None for transfers from this computer).
    pub server: Option<String>,
    /// The archive's name (server jobs).
    pub archive: String,
    /// While waiting: what the file server needs to sign in to again.
    pub link: Option<LinkTarget>,
    /// A server job relayed through this computer.
    pub relay: bool,
    /// For a relayed server job: the archive's server id (to relay it again after a pause).
    #[serde(skip)]
    pub relay_archive: Option<String>,
    /// What the transfer created that wasn't there before: project folders in
    /// the datahold (sending), or folders and files at the destination
    /// (retrieving). Abandoning it removes exactly these.
    pub created: Vec<String>,
    /// A copy checked by size and date only (an SFTP server was involved), not by checksum.
    #[serde(default)]
    pub size_only: bool,
}

/// How the window steers a transfer from this computer.
#[derive(Default)]
struct Controls {
    /// The datahold's server id.
    archive_id: String,
    /// Where it goes (a retrieve's folder on this computer).
    dest: String,
    /// See `JobView::created`.
    created: Mutex<Vec<String>>,
    /// A copy: the server it copies to (None: this computer).
    to: Option<Server>,
    /// Handed to the send or retrieve: stop at the next file.
    stop: Arc<AtomicBool>,
    cancel: AtomicBool,
    pause: AtomicBool,
}

struct Job {
    id: String,
    req: TransferRequest,
    /// The archive; for a "copy", the file server it connects to first.
    server: Server,
    /// A "copy" between two file servers: the second one.
    second: Option<Server>,
    /// The resume key: every attempt fills the same packs.
    key: String,
    ctl: Arc<Controls>,
    /// Continuing an earlier attempt (after a pause, a lost connection, or the app quitting).
    resumed: bool,
}

pub struct Jobs {
    views: Arc<Mutex<Vec<JobView>>>,
    controls: Mutex<HashMap<String, Arc<Controls>>>,
    /// Paused transfers from this computer, set aside until they're played again.
    parked: Arc<Mutex<HashMap<String, Job>>>,
    saved: Arc<Saved>,
    tx: mpsc::Sender<Job>,
    /// The next transfer number, shared by every window so ids never repeat.
    next: Arc<AtomicU64>,
    notify: Notifier,
    /// This window's name; it owns the saved transfers it has adopted or started.
    label: String,
    /// Cleared when the window closes, to stop the thread that watches server jobs.
    alive: Arc<AtomicBool>,
    /// File servers with jobs in progress, polled for updates.
    watched: Arc<Mutex<HashMap<String, Server>>>,
    /// Every file server whose jobs are shown (for pausing and cancelling).
    servers: Mutex<HashMap<String, Server>>,
    ssh: Arc<Ssh>,
    conns: Arc<Conns>,
    /// Jobs being relayed through this computer ("<server>:<job>").
    relays: Arc<Mutex<HashSet<String>>>,
    /// What each transfer started in this session works on, so the same items
    /// can't be sent (or retrieved) twice at once.
    busy: Mutex<HashMap<String, Busy>>,
}

struct Busy {
    place: String,
    sources: Vec<String>,
}

/// Where a transfer's sources live: this computer or a file server when
/// sending, the datahold when retrieving.
fn place(req: &TransferRequest) -> String {
    if req.direction == "copy" {
        format!("copy:{}", req.files_id.as_deref().unwrap_or(""))
    } else if req.direction == "send" {
        format!("send:{}", req.files_id.as_deref().unwrap_or(""))
    } else {
        format!("retrieve:{}", req.server_id)
    }
}

/// Whether two paths are the same, or one is inside the other.
fn overlaps(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim_end_matches(['/', '\\']), b.trim_end_matches(['/', '\\']));
    let inside = |x: &str, y: &str| x.len() > y.len() && x.starts_with(y) && matches!(x.as_bytes()[y.len()], b'/' | b'\\');
    a == b || inside(a, b) || inside(b, a)
}

/// The queue entry for a job running on a file server.
fn view_of(server: &Server, st: &JobStatus) -> JobView {
    JobView {
        id: format!("R:{}:{}", server.id, st.id),
        title: st.title.clone(),
        direction: st.direction.clone(),
        state: st.state.clone(),
        done: st.done,
        total: st.total,
        current: st.current.clone(),
        message: st.message.clone(),
        problems: st.problems.clone(),
        started: Some(st.started.max(0) as u64),
        rate: st.rate,
        server: Some(server.name.clone()),
        archive: st.archive.clone(),
        link: st.link.clone(),
        relay: st.relay.is_some(),
        relay_archive: st.relay.clone(),
        created: st.created.clone(),
        size_only: false,
    }
}

/// Moving now, or about to.
fn active(state: &str) -> bool {
    matches!(state, "queued" | "running" | "waiting")
}

/// Not finished: moving, or paused partway.
fn unfinished(state: &str) -> bool {
    active(state) || state == "paused"
}

/// Update the views of `server`'s jobs from a fresh listing. Returns whether
/// any job the window is showing is still in progress.
fn merge(app: &Notifier, views: &Mutex<Vec<JobView>>, server: &Server, list: &[JobStatus]) -> bool {
    let mut vs = views.lock().unwrap();
    let mut any = false;
    for st in list {
        let nv = view_of(server, st);
        if let Some(v) = vs.iter_mut().find(|v| v.id == nv.id) {
            if *v != nv {
                *v = nv.clone();
                let _ = app.emit("transfer", nv.clone());
            }
            any |= active(&nv.state);
        }
    }
    any
}

/// Whether `path` is an item directly inside the folder `dir`.
pub fn is_directly_in(path: &str, dir: &str) -> bool {
    let trim = |s: &str| s.trim_end_matches(['/', '\\']).to_string();
    path.trim_end_matches(['/', '\\']).rsplit_once(['/', '\\']).is_some_and(|(parent, _)| trim(parent) == trim(dir))
}

fn name_of(p: &str) -> String {
    p.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().filter(|s| !s.is_empty()).unwrap_or(p).to_string()
}

pub fn title(req: &TransferRequest, server: &Server) -> String {
    let first = req.sources.first().map(|s| name_of(s)).unwrap_or_default();
    let what = if req.sources.len() > 1 { format!("{first} and {} more", req.sources.len() - 1) } else { first };
    let dest = if req.dest == "/" || req.dest.is_empty() { server.name.clone() } else { name_of(&req.dest) };
    match (req.direction.as_str(), req.mode.as_str()) {
        ("send", "move") => format!("Moving {what} → {dest}"),
        ("send", _) => format!("Copying {what} → {dest}"),
        _ => format!("Retrieving {what} → {dest}"),
    }
}

impl Job {
    /// The file servers this job connects to (for copies; the archive otherwise).
    fn servers(&self) -> Vec<&Server> {
        std::iter::once(&self.server).chain(self.second.as_ref()).collect()
    }

    /// Their names, for messages.
    fn names(&self) -> String {
        self.servers().iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(" and ")
    }
}

/// The controls for a new or restored transfer.
fn controls_for(req: &TransferRequest, server: &Server, second: &Option<Server>) -> Controls {
    if req.direction == "copy" {
        // The destination server: the second one if both sides are servers, else the one connected to.
        let to = req.to_id.as_ref().map(|_| if req.files_id.is_some() { second.clone() } else { Some(server.clone()) }).flatten();
        Controls { dest: req.dest.clone(), to, ..Controls::default() }
    } else {
        Controls { archive_id: server.id.clone(), dest: req.dest.clone(), ..Controls::default() }
    }
}

fn copy_title(req: &TransferRequest, from: Option<&Server>, to: Option<&Server>) -> String {
    let first = req.sources.first().map(|s| name_of(s)).unwrap_or_default();
    let what = if req.sources.len() > 1 { format!("{first} and {} more", req.sources.len() - 1) } else { first };
    let folder = if req.dest == "/" || req.dest.is_empty() { String::new() } else { name_of(&req.dest) };
    let place = match (from, to) {
        (_, Some(t)) => format!("{} {}", t.name, folder).trim().to_string(),
        (_, None) => folder,
    };
    let verb = if req.mode == "move" { "Moving" } else { "Copying" };
    let from = from.map(|f| format!(" from {}", f.name)).unwrap_or_default();
    format!("{verb} {what}{from} → {place}")
}

fn new_view(id: &str, title: String, req: &TransferRequest, server: &Server) -> JobView {
    JobView {
        id: id.to_string(),
        title,
        direction: req.direction.clone(),
        state: "queued".into(),
        done: 0,
        total: 0,
        current: String::new(),
        message: "Waiting".into(),
        problems: Vec::new(),
        started: None,
        rate: 0,
        server: None,
        archive: if req.direction == "copy" { String::new() } else { server.name.clone() },
        link: None,
        relay: false,
        relay_archive: None,
        created: Vec::new(),
        size_only: false,
    }
}

const PAUSED: &str = "Paused. Press play to continue where it stopped; files already archived are kept.";

/// What the worker thread that runs transfers from this computer needs.
struct Worker {
    app: Notifier,
    ssh: Arc<Ssh>,
    views: Arc<Mutex<Vec<JobView>>>,
    parked: Arc<Mutex<HashMap<String, Job>>>,
    saved: Arc<Saved>,
    tx: mpsc::Sender<Job>,
}

impl Jobs {
    /// Start a window's queue. Transfers left unclaimed in `saved` (from before the app
    /// quit, or from a window that closed with them paused) come back here, paused;
    /// `servers` has their servers' current settings.
    pub fn start(notify: Notifier, ssh: Arc<Ssh>, conns: Arc<Conns>, saved: Arc<Saved>, next: Arc<AtomicU64>, servers: &[Server]) -> Jobs {
        let (tx, rx) = mpsc::channel::<Job>();
        let views: Arc<Mutex<Vec<JobView>>> = Arc::default();
        let parked: Arc<Mutex<HashMap<String, Job>>> = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let worker = Worker {
            app: notify.clone(),
            ssh: ssh.clone(),
            views: views.clone(),
            parked: parked.clone(),
            saved: saved.clone(),
            tx: tx.clone(),
        };
        std::thread::spawn(move || {
            for job in rx {
                run(&worker, job);
            }
        });
        // Watch server jobs: poll each server that has jobs in progress.
        let watched: Arc<Mutex<HashMap<String, Server>>> = Arc::default();
        let (w, v, s, c, a, alive_flag) = (watched.clone(), views.clone(), ssh.clone(), conns.clone(), notify.clone(), alive.clone());
        std::thread::spawn(move || {
            while alive_flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                let servers: Vec<Server> = w.lock().unwrap().values().cloned().collect();
                for server in servers {
                    if let Ok(list) = c.with(&s, &server, |r| r.job_list(false)) {
                        if !merge(&a, &v, &server, &list) {
                            w.lock().unwrap().remove(&server.id);
                        }
                    }
                }
            }
        });
        let jobs = Jobs {
            views,
            controls: Mutex::default(),
            parked,
            saved,
            tx,
            next,
            label: notify.label.clone(),
            notify,
            alive,
            watched,
            servers: Mutex::default(),
            ssh,
            conns,
            relays: Arc::default(),
            busy: Mutex::default(),
        };
        jobs.adopt_orphans(servers);
        jobs
    }

    /// Take on the saved transfers no window has: those that were unfinished when the app quit,
    /// and any a closed window left paused. They come back paused here.
    pub fn adopt_orphans(&self, servers: &[Server]) {
        for s in self.saved.orphans() {
            self.saved.claim(&s.id, &self.label);
            let server = servers.iter().find(|x| x.id == s.server.id).cloned().unwrap_or(s.server.clone());
            if let Some(n) = s.id.strip_prefix('L').and_then(|n| n.parse::<u64>().ok()) {
                self.next.fetch_max(n + 1, Ordering::Relaxed);
            }
            let second = s.second.as_ref().map(|x| servers.iter().find(|y| y.id == x.id).cloned().unwrap_or(x.clone()));
            let mut view = new_view(&s.id, s.title.clone(), &s.req, &server);
            view.state = "paused".into();
            view.message = "Paused when the app closed. Press play to continue where it stopped.".into();
            (view.done, view.total) = (s.done, s.total);
            view.created = s.created.clone();
            self.views.lock().unwrap().push(view.clone());
            let _ = self.notify.emit("transfer", view);
            let ctl = Arc::new(Controls { created: Mutex::new(s.created.clone()), ..controls_for(&s.req, &server, &second) });
            ctl.pause.store(true, Ordering::Relaxed);
            self.controls.lock().unwrap().insert(s.id.clone(), ctl.clone());
            self.remember(&s.id, &s.req);
            let job = Job { id: s.id.clone(), req: s.req, server, second, key: s.key, ctl, resumed: true };
            self.parked.lock().unwrap().insert(job.id.clone(), job);
        }
    }

    /// This window's transfers from this computer that are moving or waiting their turn.
    pub fn active_local_ids(&self) -> Vec<String> {
        self.views.lock().unwrap().iter().filter(|v| v.id.starts_with('L') && active(&v.state)).map(|v| v.id.clone()).collect()
    }

    /// This window is closing. Pause its transfers from this computer, waiting for them to stop,
    /// and let go of them so another window can adopt them. (Abandon ship, if that was asked for,
    /// has already dealt with the moving ones.) Transfers running on a file server are left alone.
    pub fn shutdown(&self) {
        let ids: Vec<String> =
            self.views.lock().unwrap().iter().filter(|v| v.id.starts_with('L') && unfinished(&v.state)).map(|v| v.id.clone()).collect();
        for id in &ids {
            let _ = self.pause(id);
        }
        for id in &ids {
            let _ = self.wait_stopped(id, Duration::from_secs(60));
        }
        self.saved.release(&self.label);
    }

    /// If an unfinished transfer already works on one of `req`'s items (or a
    /// folder around it, or something inside it), that item's name.
    pub fn already_transferring(&self, req: &TransferRequest) -> Option<String> {
        let place = place(req);
        let views = self.views.lock().unwrap();
        let busy = self.busy.lock().unwrap();
        views.iter().filter(|v| unfinished(&v.state)).find_map(|v| {
            let b = busy.get(&v.id).filter(|b| b.place == place)?;
            req.sources.iter().find(|s| b.sources.iter().any(|t| overlaps(s, t))).map(|s| name_of(s))
        })
    }

    /// Note what a transfer just started works on (see already_transferring).
    pub fn remember(&self, id: &str, req: &TransferRequest) {
        self.busy.lock().unwrap().insert(id.to_string(), Busy { place: place(req), sources: req.sources.clone() });
    }

    /// Show a job just started on a file server, and watch it.
    pub fn add_remote(&self, server: &Server, st: &JobStatus) -> String {
        let v = view_of(server, st);
        self.views.lock().unwrap().push(v.clone());
        let _ = self.notify.emit("transfer", v.clone());
        self.servers.lock().unwrap().insert(server.id.clone(), server.clone());
        self.watched.lock().unwrap().insert(server.id.clone(), server.clone());
        v.id
    }

    /// Show a file server's jobs found on connecting: anything unfinished or
    /// interrupted, and whatever finished in the last day (unless closed).
    pub fn adopt(&self, server: &Server, list: &[JobStatus]) {
        let recent = archive_core::util::now_secs() - 86_400;
        let dismissed = self.saved.dismissed();
        let mut vs = self.views.lock().unwrap();
        let mut any = false;
        for st in list.iter().filter(|st| unfinished(&st.state) || st.state == "interrupted" || st.updated > recent) {
            let v = view_of(server, st);
            if dismissed.contains(&v.id) {
                continue;
            }
            any |= active(&v.state);
            match vs.iter_mut().find(|x| x.id == v.id) {
                Some(x) => *x = v.clone(),
                None => vs.push(v.clone()),
            }
            let _ = self.notify.emit("transfer", v);
        }
        self.servers.lock().unwrap().insert(server.id.clone(), server.clone());
        if any {
            self.watched.lock().unwrap().insert(server.id.clone(), server.clone());
        }
    }

    pub fn submit(&self, req: TransferRequest, server: Server, second: Option<Server>) -> String {
        let id = format!("L{}", self.next.fetch_add(1, Ordering::Relaxed));
        let title = if req.direction == "copy" {
            let (from, to) = match (req.files_id.is_some(), req.to_id.is_some()) {
                (true, true) => (Some(&server), second.as_ref()),
                (true, false) => (Some(&server), None),
                _ => (None, Some(&server)),
            };
            copy_title(&req, from, to)
        } else {
            title(&req, &server)
        };
        let view = new_view(&id, title, &req, &server);
        self.views.lock().unwrap().push(view.clone());
        let _ = self.notify.emit("transfer", view.clone());
        let ctl = Arc::new(controls_for(&req, &server, &second));
        self.controls.lock().unwrap().insert(id.clone(), ctl.clone());
        self.remember(&id, &req);
        let key = format!("app-{id}-{}", archive_core::util::random_hex(4));
        self.saved.put(SavedJob {
            id: id.clone(),
            req: req.clone(),
            server: server.clone(),
            second: second.clone(),
            key: key.clone(),
            title: view.title,
            done: 0,
            total: 0,
            created: Vec::new(),
        });
        self.saved.claim(&id, &self.label);
        let _ = self.tx.send(Job { id: id.clone(), req, server, second, key, ctl, resumed: false });
        id
    }

    /// A server job's file server and its id there.
    fn remote_job(&self, id: &str) -> Result<Option<(Server, String)>, String> {
        let Some((server_id, job_id)) = id.strip_prefix("R:").and_then(|r| r.split_once(':')) else { return Ok(None) };
        let server = self.servers.lock().unwrap().get(server_id).cloned().ok_or("That server's jobs aren't available.")?;
        Ok(Some((server, job_id.to_string())))
    }

    fn state_of(&self, id: &str) -> Option<String> {
        self.views.lock().unwrap().iter().find(|v| v.id == id).map(|v| v.state.clone())
    }

    pub fn cancel(&self, id: &str) -> Result<(), String> {
        if let Some((server, job)) = self.remote_job(id)? {
            self.conns.with(&self.ssh, &server, |r| r.job_cancel(&job)).map_err(|e| e.to_string())?;
            self.watch(&server);
            return Ok(());
        }
        if let Some(c) = self.controls.lock().unwrap().get(id) {
            c.cancel.store(true, Ordering::Relaxed);
            c.stop.store(true, Ordering::Relaxed);
        }
        // Queued and paused transfers aren't running, so show them as stopped
        // now; the worker skips a queued one when it gets there.
        let parked = self.parked.lock().unwrap().remove(id).is_some();
        update(&self.notify, &self.views, id, |v| {
            if v.state == "queued" || parked {
                v.state = "cancelled".into();
                v.rate = 0;
                v.message = if parked { "Stopped. Files already archived stay archived." } else { "Cancelled before it started" }.into();
            }
        });
        if parked {
            self.saved.remove(id);
        }
        Ok(())
    }

    /// Pause a transfer: it stops at the next file and keeps its place.
    pub fn pause(&self, id: &str) -> Result<(), String> {
        if let Some((server, job)) = self.remote_job(id)? {
            self.conns.with(&self.ssh, &server, |r| r.job_pause(&job)).map_err(|e| e.to_string())?;
            self.watch(&server);
            return Ok(());
        }
        if let Some(c) = self.controls.lock().unwrap().get(id) {
            c.pause.store(true, Ordering::Relaxed);
            c.stop.store(true, Ordering::Relaxed);
        }
        // A queued one pauses at once; the worker sets it aside when it gets there.
        update(&self.notify, &self.views, id, |v| {
            if v.state == "queued" {
                v.state = "paused".into();
                v.message = PAUSED.into();
            }
        });
        Ok(())
    }

    /// Continue a paused transfer. For a relayed server job, returns its file
    /// server, archive id, and job id, to relay it again.
    pub fn resume(&self, id: &str) -> Result<Option<(Server, String, String)>, String> {
        if let Some((server, job)) = self.remote_job(id)? {
            self.conns.with(&self.ssh, &server, |r| r.job_resume(&job)).map_err(|e| e.to_string())?;
            self.watch(&server);
            let relay = self.views.lock().unwrap().iter().find(|v| v.id == id).and_then(|v| v.relay_archive.clone());
            return Ok(relay.map(|archive| (server, archive, job)));
        }
        let Some(ctl) = self.controls.lock().unwrap().get(id).cloned() else { return Ok(None) };
        if ctl.cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        ctl.pause.store(false, Ordering::Relaxed);
        ctl.stop.store(false, Ordering::Relaxed);
        let job = self.parked.lock().unwrap().remove(id);
        // Not set aside yet (still queued, or just stopping): it carries on by itself.
        update(&self.notify, &self.views, id, |v| {
            if v.state == "paused" {
                v.state = "queued".into();
                v.message = "Waiting its turn".into();
            }
        });
        if let Some(mut job) = job {
            job.resumed = true;
            let _ = self.tx.send(job);
        }
        Ok(None)
    }

    /// Transfers that could be paused, or played, right now.
    pub fn ids_in(&self, states: &[&str]) -> Vec<String> {
        self.views.lock().unwrap().iter().filter(|v| states.contains(&v.state.as_str())).map(|v| v.id.clone()).collect()
    }

    pub fn list(&self) -> Vec<JobView> {
        self.views.lock().unwrap().clone()
    }

    /// For a server job waiting for sign-in: its file server and what to sign in to.
    pub fn waiting_for(&self, id: &str) -> Option<(Server, LinkTarget, String)> {
        let (server_id, _) = id.strip_prefix("R:")?.split_once(':')?;
        let server = self.servers.lock().unwrap().get(server_id).cloned()?;
        let vs = self.views.lock().unwrap();
        let v = vs.iter().find(|v| v.id == id)?;
        Some((server, v.link.clone()?, v.archive.clone()))
    }

    /// Watch a file server's jobs again (after signing in, a waiting job resumes).
    pub fn watch(&self, server: &Server) {
        self.watched.lock().unwrap().insert(server.id.clone(), server.clone());
    }

    /// Run a file server's job through this computer (see [`crate::relay`]).
    pub fn relay(&self, files: &Server, archive: &Server, id: &str) {
        self.servers.lock().unwrap().insert(files.id.clone(), files.clone());
        self.watch(files);
        crate::relay::spawn(
            self.ssh.clone(),
            self.conns.clone(),
            self.relays.clone(),
            files.clone(),
            archive.clone(),
            id.to_string(),
        );
    }

    /// Wait until a transfer that was asked to stop has stopped.
    pub fn wait_stopped(&self, id: &str, limit: Duration) -> Result<(), String> {
        let start = Instant::now();
        while self.state_of(id).is_some_and(|s| active(&s)) {
            if start.elapsed() > limit {
                return Err("It's still stopping. Try again in a moment.".into());
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        Ok(())
    }

    /// What abandoning a stopped transfer involves.
    pub fn abandon_plan(&self, id: &str) -> Result<Abandon, String> {
        let view = self.views.lock().unwrap().iter().find(|v| v.id == id).cloned().ok_or("That transfer isn't in the list any more.")?;
        if let Some((server, job)) = self.remote_job(id)? {
            return Ok(Abandon {
                direction: view.direction,
                created: view.created,
                archive_id: view.relay_archive,
                archive_name: view.archive,
                files: Some((server, job)),
                local_dest: None,
                copy_dest: None,
            });
        }
        let ctl = self.controls.lock().unwrap().get(id).cloned().ok_or("That transfer isn't in the list any more.")?;
        let created = ctl.created.lock().unwrap().clone();
        let is_copy = view.direction == "copy";
        Ok(Abandon {
            direction: view.direction,
            created,
            archive_id: Some(ctl.archive_id.clone()),
            archive_name: view.archive,
            files: None,
            local_dest: (!is_copy).then(|| ctl.dest.clone()),
            copy_dest: is_copy.then(|| (ctl.dest.clone(), ctl.to.clone())),
        })
    }

    /// Show a transfer as abandoned, and forget it.
    pub fn abandoned(&self, id: &str, message: String) {
        update(&self.notify, &self.views, id, |v| {
            v.state = "cancelled".into();
            v.rate = 0;
            v.message = message;
            v.created.clear();
        });
        self.saved.remove(id);
    }

    /// Close one finished transfer's row.
    pub fn dismiss(&self, id: &str) {
        if self.state_of(id).is_some_and(|s| unfinished(&s)) {
            return;
        }
        self.views.lock().unwrap().retain(|v| v.id != id);
        if id.starts_with("R:") {
            self.saved.dismiss(&[id.to_string()]);
        }
    }

    pub fn clear_finished(&self) {
        let mut closed = Vec::new();
        self.views.lock().unwrap().retain(|v| {
            let keep = unfinished(&v.state);
            if !keep && v.id.starts_with("R:") {
                closed.push(v.id.clone());
            }
            keep
        });
        self.saved.dismiss(&closed);
    }
}

/// What closing a window would interrupt, for the warning.
#[derive(Serialize, Default, Clone)]
pub struct Leaving {
    /// Transfers from this computer that are moving or waiting their turn.
    pub transfers: Vec<String>,
    /// Transfers from this computer already paused (they stay paused).
    pub paused: usize,
    /// Transfers running on file servers, by server: these carry on.
    pub servers: Vec<(String, usize)>,
    /// Servers this window (or, for quitting, any window) is connected to; closing ends those.
    pub connections: Vec<String>,
}

impl Jobs {
    pub fn leaving(&self) -> Leaving {
        let mut out = Leaving::default();
        for v in self.views.lock().unwrap().iter() {
            if v.id.starts_with('L') {
                if active(&v.state) {
                    out.transfers.push(v.title.clone());
                } else if v.state == "paused" {
                    out.paused += 1;
                }
            } else if active(&v.state) {
                if let Some(name) = &v.server {
                    match out.servers.iter_mut().find(|(n, _)| n == name) {
                        Some(e) => e.1 += 1,
                        None => out.servers.push((name.clone(), 1)),
                    }
                }
            }
        }
        out
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
    }
}

fn update(app: &Notifier, views: &Mutex<Vec<JobView>>, id: &str, f: impl FnOnce(&mut JobView)) {
    let mut vs = views.lock().unwrap();
    if let Some(v) = vs.iter_mut().find(|v| v.id == id) {
        f(v);
        let _ = app.emit("transfer", v.clone());
    }
}

// ---------------------------------------------------------------------------
// Saved transfers
// ---------------------------------------------------------------------------

/// Unfinished transfers from this computer (and closed server-job rows),
/// kept in a JSON file in the app's config folder.
pub struct Saved {
    path: PathBuf,
    file: Mutex<SavedFile>,
    /// Which window has each saved transfer (in memory only: after a restart none does).
    owners: Mutex<HashMap<String, String>>,
}

#[derive(Default, Serialize, Deserialize)]
struct SavedFile {
    #[serde(default)]
    jobs: Vec<SavedJob>,
    /// Server jobs whose rows were closed, so they don't come back on reconnecting.
    #[serde(default)]
    dismissed: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedJob {
    id: String,
    req: TransferRequest,
    server: Server,
    #[serde(default)]
    second: Option<Server>,
    key: String,
    title: String,
    done: u64,
    total: u64,
    #[serde(default)]
    created: Vec<String>,
}

/// See `Jobs::abandon_plan`.
pub struct Abandon {
    pub direction: String,
    pub created: Vec<String>,
    /// The datahold's server id, when known (otherwise find it by name).
    pub archive_id: Option<String>,
    pub archive_name: String,
    /// A server job: its file server and id there.
    pub files: Option<(Server, String)>,
    /// A retrieve to this computer: its destination folder.
    pub local_dest: Option<String>,
    /// A copy: its destination folder, and the server that's on (None: this computer).
    pub copy_dest: Option<(String, Option<Server>)>,
}

impl Saved {
    pub fn load(path: PathBuf) -> Saved {
        let file = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Saved { path, file: Mutex::new(file), owners: Mutex::default() }
    }

    fn change(&self, f: impl FnOnce(&mut SavedFile)) {
        let mut file = self.file.lock().unwrap();
        f(&mut file);
        // Written to a temporary file, then renamed over the old one, so a crash never leaves half a file.
        if let (Ok(bytes), Some(dir)) = (serde_json::to_vec_pretty(&*file), self.path.parent()) {
            let tmp = self.path.with_extension("json.tmp");
            let _ = std::fs::create_dir_all(dir);
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
    }

    #[cfg(test)]
    fn jobs(&self) -> Vec<SavedJob> {
        self.file.lock().unwrap().jobs.clone()
    }

    /// The saved transfers no window has.
    fn orphans(&self) -> Vec<SavedJob> {
        let owners = self.owners.lock().unwrap();
        self.file.lock().unwrap().jobs.iter().filter(|j| !owners.contains_key(&j.id)).cloned().collect()
    }

    fn claim(&self, id: &str, window: &str) {
        self.owners.lock().unwrap().insert(id.to_string(), window.to_string());
    }

    /// A window is closing: its saved transfers are nobody's until another window adopts them.
    pub fn release(&self, window: &str) {
        self.owners.lock().unwrap().retain(|_, w| w != window);
    }

    fn dismissed(&self) -> HashSet<String> {
        self.file.lock().unwrap().dismissed.iter().cloned().collect()
    }

    fn put(&self, job: SavedJob) {
        self.change(|f| {
            f.jobs.retain(|j| j.id != job.id);
            f.jobs.push(job);
        });
    }

    fn remove(&self, id: &str) {
        if self.file.lock().unwrap().jobs.iter().any(|j| j.id == id) {
            self.change(|f| f.jobs.retain(|j| j.id != id));
        }
    }

    fn progress(&self, id: &str, done: u64, total: u64) {
        self.change(|f| {
            if let Some(j) = f.jobs.iter_mut().find(|j| j.id == id) {
                (j.done, j.total) = (done, total);
            }
        });
    }

    fn created(&self, id: &str, created: &[String]) {
        self.change(|f| {
            if let Some(j) = f.jobs.iter_mut().find(|j| j.id == id) {
                j.created = created.to_vec();
            }
        });
    }

    fn dismiss(&self, ids: &[String]) {
        if !ids.is_empty() {
            self.change(|f| f.dismissed.extend(ids.iter().cloned()));
        }
    }
}

// ---------------------------------------------------------------------------
// Running transfers from this computer
// ---------------------------------------------------------------------------

enum Outcome {
    Finished,
    /// Paused partway; it can continue from where it stopped.
    Paused,
    /// The connection dropped; the transfer can continue from where it stopped.
    Lost,
}

fn run(w: &Worker, job: Job) {
    let ctl = job.ctl.clone();
    if ctl.cancel.load(Ordering::Relaxed) {
        update(&w.app, &w.views, &job.id, |v| {
            v.state = "cancelled".into();
            v.message = "Cancelled before it started".into();
        });
        w.saved.remove(&job.id);
        return;
    }
    if ctl.pause.load(Ordering::Relaxed) {
        return park(w, job, PAUSED);
    }
    let resumed = job.resumed;
    update(&w.app, &w.views, &job.id, |v| {
        v.state = "running".into();
        v.message = if resumed { "Continuing where it stopped" } else { "Connecting" }.into();
        v.started.get_or_insert_with(|| archive_core::util::now_secs().max(0) as u64);
    });
    // A transfer played again continues where it stopped: files already done are kept.
    let outcome = if job.req.direction == "copy" {
        // Each side is a helper session or, for an SFTP server, an SFTP session.
        let ends = (|| -> archive_core::Result<_> {
            let first = connect_end(w, &job.server)?;
            let second = match &job.second {
                Some(s) => Some(connect_end(w, s)?),
                None => None,
            };
            Ok((first, second))
        })();
        match ends {
            Ok((mut first, mut second)) => {
                let second: Option<&mut dyn archive_core::copy::Endpoint> = match second.as_mut() {
                    Some(b) => Some(b.as_mut()),
                    None => None,
                };
                transfer_copy(w, &job, first.as_mut(), second, resumed)
            }
            Err(e) => return not_connected(w, job, e, resumed),
        }
    } else {
        match connect(w, &job.server) {
            Ok(mut remote) => transfer(w, &job, &mut remote, resumed),
            Err(e) => return not_connected(w, job, e, resumed),
        }
    };
    match outcome {
        Outcome::Finished => w.saved.remove(&job.id),
        Outcome::Paused => park(w, job, PAUSED),
        // Pause rather than wait: press play once you're back online.
        Outcome::Lost => {
            let why = format!("Lost the connection to {}. Press play to reconnect and continue where it stopped.", job.names());
            park(w, job, &why)
        }
    }
}

/// One connection to `server`, asking for a password if it needs one.
fn connect(w: &Worker, server: &Server) -> archive_core::Result<Remote> {
    Remote::ssh_with(&server.target(), &w.ssh.setup(server))
}

/// One side of a copy: a helper session, or an SFTP session for an SFTP server.
fn connect_end(w: &Worker, server: &Server) -> archive_core::Result<Box<dyn archive_core::copy::Endpoint>> {
    if server.kind == crate::settings::ServerKind::Sftp {
        let mut sftp = archive_core::sftp::Sftp::ssh(&server.destination(), server.port, &w.ssh.setup(server))?;
        sftp.read_back = server.read_back;
        Ok(Box::new(sftp))
    } else {
        Ok(Box::new(connect(w, server)?))
    }
}

/// The transfer couldn't connect. Played again after a stop or a lost connection, it pauses again
/// (so it can be tried once more) instead of failing.
fn not_connected(w: &Worker, job: Job, e: archive_core::Error, resumed: bool) {
    if resumed {
        let why = format!("Couldn't reconnect to {}: {e}. Press play to try again.", job.names());
        return park(w, job, &why);
    }
    update(&w.app, &w.views, &job.id, |v| {
        v.state = "failed".into();
        v.message = format!("Couldn't connect: {e}");
    });
    w.saved.remove(&job.id);
}

/// Set a paused transfer aside until it's played again, so the queue moves on.
fn park(w: &Worker, mut job: Job, message: &str) {
    // Played again while it was stopping: carry on instead.
    if !job.ctl.pause.load(Ordering::Relaxed) && !job.ctl.cancel.load(Ordering::Relaxed) && message == PAUSED {
        job.resumed = true;
        update(&w.app, &w.views, &job.id, |v| {
            v.state = "queued".into();
            v.message = "Waiting its turn".into();
        });
        let _ = w.tx.send(job);
        return;
    }
    job.ctl.pause.store(true, Ordering::Relaxed);
    let mut progress = (0, 0);
    update(&w.app, &w.views, &job.id, |v| {
        v.state = "paused".into();
        v.rate = 0;
        v.message = message.into();
        progress = (v.done, v.total);
    });
    w.saved.progress(&job.id, progress.0, progress.1);
    w.parked.lock().unwrap().insert(job.id.clone(), job);
}

/// One attempt at the whole transfer. Sending again skips files already
/// archived; retrieving again skips files already here.
fn transfer(w: &Worker, job: &Job, remote: &mut Remote, resumed: bool) -> Outcome {
    let (app, views) = (&w.app, &*w.views);
    // Progress callbacks fire often; send the window at most ~5 updates a second,
    // and save it (for after a quit) every half minute.
    let mut last = Instant::now() - Duration::from_secs(1);
    let mut last_saved = Instant::now();
    let mut current = String::new();
    // Speed over the last ten seconds, so it shows what's happening now.
    let mut speed = archive_core::util::Speed::new(Duration::from_secs(10));
    let mut progress = |done: u64, total: u64, current: &str, force: bool| {
        if force || last.elapsed() >= Duration::from_millis(200) {
            last = Instant::now();
            let rate = speed.update(done);
            update(app, views, &job.id, |v| {
                v.done = done;
                v.total = total;
                v.current = current.to_string();
                v.rate = rate;
                v.message = "Transferring".into();
            });
        }
        if last_saved.elapsed() >= Duration::from_secs(30) {
            last_saved = Instant::now();
            w.saved.progress(&job.id, done, total);
        }
    };
    // Stopped at the user's request, and not cancelled: paused.
    let paused = || !job.ctl.cancel.load(Ordering::Relaxed);
    // Note something this transfer created (see `JobView::created`).
    let created = |path: String| {
        let list = {
            let mut c = job.ctl.created.lock().unwrap();
            if c.contains(&path) {
                return;
            }
            c.push(path);
            c.clone()
        };
        w.saved.created(&job.id, &list);
        update(app, views, &job.id, |v| v.created = list);
    };

    let req = &job.req;
    if req.direction == "send" {
        let opts = SendOptions {
            mode: if req.mode == "move" { Mode::Move } else { Mode::Copy },
            policy: match req.conflict.as_str() {
                "replace" => Conflict::Replace,
                "keep-both" => Conflict::KeepBoth,
                _ => Conflict::Skip,
            },
            job: job.key.clone(),
            cancel: Some(job.ctl.stop.clone()),
            new_project: req.new_project.clone(),
            ..SendOptions::default()
        };
        // In Move mode, files already archived were removed; send what's left.
        let sources: Vec<PathBuf> = req.sources.iter().map(PathBuf::from).filter(|p| !resumed || p.symlink_metadata().is_ok()).collect();
        let dest = VPath::parse(&req.dest).unwrap_or_default();
        let mut total = 0;
        let mut done_so_far = 0;
        let res = send::send(remote, &sources, &dest, &opts, &mut |e| match e {
            send::Event::Scanned { bytes, .. } => {
                total = *bytes;
                progress(0, total, "", true);
            }
            send::Event::Sending { path } | send::Event::Checking { path } => current = name_of(&path.display().to_string()),
            send::Event::Verifying { path } => {
                // The datahold reads a large file back to check it; say so rather than look stalled.
                current = format!("{} (being checked)", name_of(&path.display().to_string()));
                progress(done_so_far, total, &current, true);
            }
            send::Event::Progress { done, total } => {
                done_so_far = *done;
                progress(*done, *total, &current, false)
            }
            send::Event::Sealing => progress(total, total, "Finishing", true),
            send::Event::Project { path, new: true } => created(path.to_string()),
            _ => {}
        });
        match res {
            Err(e) if e.is_lost() => return Outcome::Lost,
            Ok(r) if r.cancelled && paused() => {
                let _ = remote.kick_worker();
                return Outcome::Paused;
            }
            res => {
                let _ = remote.kick_worker();
                finish_send(app, views, &job.id, res, opts.mode == Mode::Move);
            }
        }
    } else {
        let opts = RetrieveOptions {
            policy: match req.conflict.as_str() {
                // Resuming: files already retrieved are kept, not fetched again.
                _ if resumed => LocalConflict::Skip,
                "replace" => LocalConflict::Replace,
                "keep-both" => LocalConflict::KeepBoth,
                _ => LocalConflict::Skip,
            },
            cancel: Some(job.ctl.stop.clone()),
            ..RetrieveOptions::default()
        };
        let dest = PathBuf::from(&req.dest);
        let mut report = retrieve::RetrieveReport::default();
        let mut error = None;
        for src in &req.sources {
            let Ok(src) = VPath::parse(src) else { continue };
            // Note what this creates, before it does (a resumed transfer keeps what it noted).
            if let Some(name) = src.name() {
                let target = dest.join(name);
                if !target.exists() {
                    created(target.display().to_string());
                }
            }
            let base_done = report.bytes;
            let res = retrieve::retrieve(remote, &src, &dest, &opts, &mut |e| match e {
                retrieve::Event::Planned { .. } => {}
                retrieve::Event::Receiving { path } => current = name_of(&path.display().to_string()),
                retrieve::Event::Progress { done, total } => progress(base_done + done, base_done + total, &current, false),
                _ => {}
            });
            match res {
                Ok(r) => {
                    report.files += r.files;
                    report.bytes += r.bytes;
                    report.written += r.written;
                    report.skipped.extend(r.skipped);
                    report.failed.extend(r.failed);
                    report.renamed.extend(r.renamed);
                    report.cancelled |= r.cancelled;
                }
                Err(e) if e.is_lost() => return Outcome::Lost,
                Err(e) => error = Some(e.to_string()),
            }
            if job.ctl.stop.load(Ordering::Relaxed) {
                report.cancelled = true;
                break;
            }
        }
        if report.cancelled && error.is_none() && paused() {
            return Outcome::Paused;
        }
        finish_retrieve(app, views, &job.id, report, error);
    }
    Outcome::Finished
}

/// One attempt at a plain copy or move (Transfer mode). Running it again
/// skips files already there and continues a half-written file.
fn transfer_copy(
    w: &Worker,
    job: &Job,
    first: &mut dyn archive_core::copy::Endpoint,
    second: Option<&mut dyn archive_core::copy::Endpoint>,
    resumed: bool,
) -> Outcome {
    use archive_core::copy::{self, CopyOptions, Endpoint, Event, LocalEnd};
    let (app, views) = (&w.app, &*w.views);
    let req = &job.req;
    let mut here_a = LocalEnd::new();
    let mut here_b = LocalEnd::new();
    let (src, dst): (&mut dyn Endpoint, &mut dyn Endpoint) = match (req.files_id.is_some(), req.to_id.is_some(), second) {
        (true, true, Some(s2)) => (first, s2),
        (true, false, _) => (first, &mut here_b),
        (false, true, _) => (&mut here_a, first),
        _ => {
            update(app, views, &job.id, |v| {
                v.state = "failed".into();
                v.message = "Choose a file server for at least one side.".into();
            });
            return Outcome::Finished;
        }
    };
    let moving = req.mode == "move";
    let opts = CopyOptions {
        mode: if moving { Mode::Move } else { Mode::Copy },
        policy: match req.conflict.as_str() {
            "replace" => LocalConflict::Replace,
            "keep-both" => LocalConflict::KeepBoth,
            _ => LocalConflict::Skip,
        },
        // What an earlier attempt made here, so a continued Keep both fills "X (2)" again.
        reuse: if resumed { job.ctl.created.lock().unwrap().clone() } else { Vec::new() },
        cancel: Some(job.ctl.stop.clone()),
        ..CopyOptions::default()
    };

    // Progress callbacks fire often; tell the window at most ~5 times a second,
    // and save it (for after a quit) every half minute.
    let mut last = Instant::now() - Duration::from_secs(1);
    let mut last_saved = Instant::now();
    let mut speed = archive_core::util::Speed::new(Duration::from_secs(10));
    let (mut current, mut writing_to) = (String::new(), None::<String>);
    let mut progress = |done: u64, total: u64, current: &str, force: bool| {
        if force || last.elapsed() >= Duration::from_millis(200) {
            last = Instant::now();
            let rate = speed.update(done);
            update(app, views, &job.id, |v| {
                v.done = done;
                v.total = total;
                v.current = current.to_string();
                v.rate = rate;
                v.message = "Transferring".into();
            });
        }
        if last_saved.elapsed() >= Duration::from_secs(30) {
            last_saved = Instant::now();
            w.saved.progress(&job.id, done, total);
        }
    };
    let res = copy::copy(src, dst, &req.sources, &req.dest, &opts, &mut |e| match e {
        Event::Scanned { bytes, .. } => progress(0, *bytes, "", true),
        Event::Copying { path, to } => {
            current = name_of(path);
            writing_to = Some(to.clone());
        }
        Event::Progress { done, total } => progress(*done, *total, &current, false),
        // Note what this creates at the top of the destination (see `JobView::created`), so
        // Abandon can remove it. A Move is a Copy until every file has arrived, so it can be
        // undone the same way, up to the moment it starts deleting originals.
        Event::Created { path, .. } if is_directly_in(path, &req.dest) => {
            let list = {
                let mut c = job.ctl.created.lock().unwrap();
                if !c.contains(path) {
                    c.push(path.clone());
                }
                c.clone()
            };
            w.saved.created(&job.id, &list);
            update(app, views, &job.id, |v| v.created = list);
        }
        Event::Checking { path } => {
            current = format!("{} (being checked)", name_of(path));
            update(app, views, &job.id, |v| v.current = current.clone());
        }
        // A Move starts deleting originals only after everything has arrived. From here on
        // the copies are the only copies, so there is nothing left to undo.
        Event::Removed { .. } => {
            let mut c = job.ctl.created.lock().unwrap();
            if !c.is_empty() {
                c.clear();
                drop(c);
                w.saved.created(&job.id, &[]);
                update(app, views, &job.id, |v| v.created.clear());
            }
        }
        _ => {}
    });
    match res {
        Err(e) if e.is_lost() => Outcome::Lost,
        // Stopped at the user's request, and not cancelled: paused. The half-written file is kept.
        Ok(r) if r.cancelled && !job.ctl.cancel.load(Ordering::Relaxed) => Outcome::Paused,
        res => {
            // Cancelled or abandoned: the half-written file is no use to anyone.
            if job.ctl.cancel.load(Ordering::Relaxed) {
                if let Some(to) = writing_to {
                    let _ = dst.remove(&copy::part_string(&to));
                }
            }
            finish_copy(app, views, &job.id, res, moving);
            Outcome::Finished
        }
    }
}

fn finish_copy(app: &Notifier, views: &Mutex<Vec<JobView>>, id: &str, res: archive_core::Result<archive_core::copy::CopyReport>, moving: bool) {
    update(app, views, id, |v| match res {
        Err(e) => {
            v.state = "failed".into();
            v.message = e.to_string();
        }
        Ok(r) => {
            let there = r.skipped.iter().filter(|(_, why)| why == "already there").count();
            let other = r.skipped.len() - there;
            let mut msg = format!("{} of {} files {} and verified", r.copied, r.files, if moving { "moved" } else { "copied" });
            if there > 0 {
                msg.push_str(&format!("; {there} already there"));
            }
            if !r.kept_both.is_empty() {
                msg.push_str(&format!("; {} kept beside the one already there", r.kept_both.len()));
            }
            if r.originals_kept {
                msg.push_str("; the originals were kept, since not everything was copied");
            } else if moving && r.removed > 0 {
                msg.push_str("; originals removed after everything was verified");
            }
            if other > 0 {
                msg.push_str(&format!("; {other} skipped"));
            }
            v.problems = r
                .failed
                .iter()
                .map(|(p, e)| format!("{p}: {e}"))
                .chain(r.kept.iter().map(|(p, e)| format!("Kept {p}: {e}")))
                .chain(r.skipped.iter().filter(|(_, why)| why != "already there").map(|(p, why)| format!("Skipped {p}: {why}")))
                .take(200)
                .collect();
            v.size_only = r.size_only;
            v.state = if r.cancelled {
                msg = format!("Stopped. {msg}");
                "cancelled".into()
            } else if r.failed.is_empty() {
                "done".into()
            } else {
                msg.push_str(&format!("; {} failed", r.failed.len()));
                "failed".into()
            };
            v.message = msg;
            v.done = v.total;
        }
    });
}

fn finish_send(app: &Notifier, views: &Mutex<Vec<JobView>>, id: &str, res: archive_core::Result<send::SendReport>, moved: bool) {
    update(app, views, id, |v| match res {
        Err(e) => {
            v.state = "failed".into();
            v.message = e.to_string();
        }
        Ok(r) => {
            let archived = r.stored + r.deduplicated + r.identical;
            let mut msg = format!("{archived} of {} files archived and verified", r.files);
            if r.originals_kept {
                msg.push_str("; the originals were kept, since not everything was archived");
            } else if moved {
                msg.push_str(&format!("; {} removed from this computer", r.deleted));
            }
            if !r.skipped.is_empty() {
                msg.push_str(&format!("; {} skipped (a different item already has that name)", r.skipped.len()));
            }
            v.problems = r
                .failed
                .iter()
                .map(|(p, e)| format!("{}: {e}", p.display()))
                .chain(r.kept.iter().map(|(p, e)| format!("Kept {}: {e}", p.display())))
                .take(200)
                .collect();
            v.state = if r.cancelled {
                "cancelled".into()
            } else if r.failed.is_empty() {
                "done".into()
            } else {
                "failed".into()
            };
            if r.cancelled {
                msg = format!("Stopped. {msg}");
            } else if !r.failed.is_empty() {
                msg.push_str(&format!("; {} failed", r.failed.len()));
            }
            msg.push_str(&format!(" ({} sent)", human_bytes(r.bytes_sent)));
            v.message = msg;
            v.done = v.total;
        }
    });
}

fn finish_retrieve(app: &Notifier, views: &Mutex<Vec<JobView>>, id: &str, r: retrieve::RetrieveReport, error: Option<String>) {
    update(app, views, id, |v| {
        v.problems = r.failed.iter().map(|(p, e)| format!("{}: {e}", p.display())).take(200).collect();
        if let Some(e) = error {
            v.state = "failed".into();
            v.message = e;
            return;
        }
        let mut msg = format!("{} of {} files retrieved and verified", r.written, r.files);
        if !r.skipped.is_empty() {
            msg.push_str(&format!("; {} already here (skipped)", r.skipped.len()));
        }
        if !r.renamed.is_empty() {
            msg.push_str(&format!("; {} renamed for this system", r.renamed.len()));
        }
        v.state = if r.cancelled {
            msg = format!("Stopped. {msg}");
            "cancelled".into()
        } else if r.failed.is_empty() {
            "done".into()
        } else {
            msg.push_str(&format!("; {} failed", r.failed.len()));
            "failed".into()
        };
        v.message = msg;
        v.done = v.total;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> TransferRequest {
        TransferRequest {
            direction: "send".into(),
            server_id: "hill".into(),
            files_id: None,
            to_id: None,
            sources: vec!["/Users/me/Research/run42".into()],
            dest: "/Projects".into(),
            mode: "move".into(),
            conflict: "skip".into(),
            new_project: None,
            relay: false,
        }
    }

    fn server() -> Server {
        serde_json::from_str(r#"{"id":"hill","name":"Lab datahold","host":"hill","root":"/mnt/archive"}"#).unwrap()
    }

    #[test]
    fn unfinished_transfers_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transfers.json");
        let saved = Saved::load(path.clone());
        let job = |id: &str| SavedJob { id: id.into(), req: req(), server: server(), second: None, key: format!("app-{id}-ab12"), title: "Moving run42".into(), done: 0, total: 0, created: Vec::new() };
        saved.put(job("L1"));
        saved.put(job("L2"));
        saved.progress("L1", 40, 100);
        saved.created("L1", &["/Projects/run42".to_string()]);
        saved.remove("L2");
        saved.dismiss(&["R:compute:17".into()]);

        let again = Saved::load(path);
        let jobs = again.jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!((jobs[0].id.as_str(), jobs[0].done, jobs[0].total), ("L1", 40, 100));
        assert_eq!(jobs[0].key, "app-L1-ab12", "the resume key is kept, so the same packs are filled");
        assert_eq!(jobs[0].created, vec!["/Projects/run42".to_string()], "what it created is remembered, for abandoning later");
        assert!(again.dismissed().contains("R:compute:17"));
    }

    #[test]
    fn saved_transfers_belong_to_one_window_until_it_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let saved = Saved::load(dir.path().join("transfers.json"));
        let job = |id: &str| SavedJob { id: id.into(), req: req(), server: server(), second: None, key: format!("app-{id}"), title: "t".into(), done: 0, total: 0, created: Vec::new() };
        saved.put(job("L1"));
        saved.put(job("L2"));
        // After a restart nobody has them.
        assert_eq!(saved.orphans().len(), 2);
        saved.claim("L1", "main");
        saved.claim("L2", "w2");
        assert!(saved.orphans().is_empty(), "a window that has a transfer keeps it");
        // A window closes with its transfer paused: it becomes free for another window.
        saved.release("w2");
        let free: Vec<String> = saved.orphans().into_iter().map(|j| j.id).collect();
        assert_eq!(free, ["L2"]);
        assert_eq!(saved.jobs().len(), 2, "nothing is lost from the saved file");
    }

    #[test]
    fn overlapping_paths() {
        assert!(overlaps("/a/b", "/a/b/"));
        assert!(overlaps("/a/b/c", "/a/b"));
        assert!(!overlaps("/a/bc", "/a/b"));
    }
}
