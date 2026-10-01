//! Archive desktop app: two panes (this computer and a server), a transfer
//! queue, and settings. The window's interface lives in `app/ui`; this is the
//! backend it calls.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod askpass;
mod conn;
mod jobs;
mod local;
mod relay;
mod settings;

use std::path::PathBuf;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use archive_core::catalog::{Entry, NodeKind};
use archive_core::jobs::{ArchiveTarget, SshAuth};
use archive_core::link::LinkTarget;
use archive_core::maint::Info;
use archive_core::proto::InitOptions;
use archive_core::{Archive, VPath};
use serde::Serialize;
use tauri::menu::{Menu, MenuItemBuilder, MenuItemKind, PredefinedMenuItem};
use tauri::{Emitter, Manager, State};

use conn::{ConnInfo, Conns, Ssh};
use jobs::{JobView, Jobs, Leaving, TransferRequest};
use local::{Crumb, Item, Listing, Measure, Place};
use settings::{Server, Settings};

/// Everything one window owns: its connections, its transfer queue, and how it signs in.
/// (Commands find theirs by window; see [`Registry::session`].)
struct Session {
    settings_path: PathBuf,
    settings: Arc<Mutex<Settings>>,
    ssh: Arc<Ssh>,
    conns: Arc<Conns>,
    jobs: Jobs,
    helpers_dir: PathBuf,
    /// Bumped by each new search in this window (and by cancelling), which stops its older ones.
    search_gen: Arc<AtomicU64>,
}

/// What every window shares: the settings, sign-in questions, the transfers saved to disk, and
/// the sessions themselves. Windows can't see each other's connections or transfers.
struct Registry {
    app: tauri::AppHandle,
    settings_path: PathBuf,
    settings: Arc<Mutex<Settings>>,
    helpers_dir: PathBuf,
    askpass: askpass::Askpass,
    saved: Arc<jobs::Saved>,
    next_transfer: Arc<AtomicU64>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

type St<'a> = State<'a, Arc<Registry>>;

impl Registry {
    /// The window's session, made the first time the window needs one.
    fn session(&self, label: &str) -> Arc<Session> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(se) = sessions.get(label) {
            return se.clone();
        }
        let ssh = Arc::new(Ssh::new(self.askpass.clone(), label.to_string()));
        let conns = Arc::new(Conns::default());
        let servers = self.settings.lock().unwrap().servers.clone();
        let notify = jobs::Notifier::new(self.app.clone(), label);
        let jobs = Jobs::start(notify, ssh.clone(), conns.clone(), self.saved.clone(), self.next_transfer.clone(), &servers);
        let se = Arc::new(Session {
            settings_path: self.settings_path.clone(),
            settings: self.settings.clone(),
            ssh,
            conns,
            jobs,
            helpers_dir: self.helpers_dir.clone(),
            search_gen: Arc::default(),
        });
        // A window that's already gone (a late command) gets a session that isn't kept.
        if self.app.get_webview_window(label).is_some() {
            sessions.insert(label.to_string(), se.clone());
        }
        se
    }

    fn all_sessions(&self) -> Vec<Arc<Session>> {
        self.sessions.lock().unwrap().values().cloned().collect()
    }

    /// If any window is already transferring one of these items, that item's name.
    fn already_transferring(&self, req: &TransferRequest) -> Option<String> {
        self.all_sessions().iter().find_map(|se| se.jobs.already_transferring(req))
    }

    /// A window is closing: pause its transfers from this computer, for another window to adopt,
    /// and let go of its connections.
    fn close_session(&self, label: &str) {
        let session = self.sessions.lock().unwrap().remove(label);
        if let Some(se) = session {
            se.jobs.shutdown();
        }
        self.rehome_orphans();
    }

    /// What closing this window would interrupt (`None` if nothing).
    fn leaving(&self, label: &str) -> Option<Leaving> {
        let se = self.sessions.lock().unwrap().get(label).cloned()?;
        let mut out = se.jobs.leaving();
        if out.transfers.is_empty() {
            return None;
        }
        out.connections = self.server_names(&se.conns.open_ids());
        Some(out)
    }

    /// What quitting would interrupt, across every window (`None` if nothing).
    fn leaving_app(&self) -> Option<Leaving> {
        let mut all = Leaving::default();
        for se in self.all_sessions() {
            let one = se.jobs.leaving();
            all.transfers.extend(one.transfers);
            all.paused += one.paused;
            for (name, n) in one.servers {
                match all.servers.iter_mut().find(|(m, _)| *m == name) {
                    Some(e) => e.1 += n,
                    None => all.servers.push((name, n)),
                }
            }
            for name in self.server_names(&se.conns.open_ids()) {
                if !all.connections.contains(&name) {
                    all.connections.push(name);
                }
            }
        }
        (!all.transfers.is_empty()).then_some(all)
    }

    fn server_names(&self, ids: &[String]) -> Vec<String> {
        let settings = self.settings.lock().unwrap();
        let mut names: Vec<String> = ids.iter().filter_map(|id| settings.servers.iter().find(|s| &s.id == id).map(|s| s.name.clone())).collect();
        names.sort();
        names
    }

    /// Hand transfers no window has (from one that just closed) to a window that's still open.
    fn rehome_orphans(&self) {
        let front = self.app.webview_windows().into_values().find(|w| w.is_focused().unwrap_or(false)).map(|w| w.label().to_string());
        let sessions = self.sessions.lock().unwrap();
        let se = front.and_then(|l| sessions.get(&l)).or_else(|| sessions.values().next());
        if let Some(se) = se {
            let servers = self.settings.lock().unwrap().servers.clone();
            se.jobs.adopt_orphans(&servers);
        }
    }
}

impl Session {
    fn server(&self, id: &str) -> Result<Server, String> {
        self.settings
            .lock()
            .unwrap()
            .servers
            .iter()
            .find(|s| s.id == id)
            .cloned()
            .ok_or_else(|| "That server isn't in Settings any more.".to_string())
    }
}

/// Do something on a file server, through the helper or, for an SFTP server, over SFTP.
fn on_server<T>(
    st: &Session,
    server: &Server,
    files: impl Fn(&mut archive_core::remote::Remote) -> archive_core::Result<T>,
    sftp: impl Fn(&mut archive_core::sftp::Sftp) -> archive_core::Result<T>,
) -> Result<T, String> {
    if server.kind == settings::ServerKind::Sftp { st.conns.with_sftp(&st.ssh, server, sftp) } else { st.conns.with(&st.ssh, server, files) }.map_err(err)
}

/// Run blocking work off the window's thread.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

fn err(e: archive_core::Error) -> String {
    use archive_core::Error::*;
    match e {
        NotFound(m) => format!("Not found: {m}"),
        AlreadyExists(m) => format!("Something named {m} already exists."),
        InvalidPath(m) => m,
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Settings and this computer
// ---------------------------------------------------------------------------

#[tauri::command]
fn settings_get(state: St) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn settings_save(app: tauri::AppHandle, state: St, mut settings: Settings) -> Result<Settings, String> {
    for s in &mut settings.servers {
        if s.id.is_empty() {
            s.id = archive_core::util::random_hex(6);
        }
        s.name = s.name.trim().to_string();
        if s.name.is_empty() {
            s.name = s.host.clone();
        }
    }
    settings::save(&state.settings_path, &settings).map_err(|e| e.to_string())?;
    // Server details may have changed; reconnect on next use.
    for se in state.all_sessions() {
        for s in &settings.servers {
            se.conns.forget(&s.id);
        }
    }
    *state.settings.lock().unwrap() = settings.clone();
    // Other windows show the same servers.
    let _ = app.emit("settings-changed", &settings);
    Ok(settings)
}

#[tauri::command]
fn places() -> Vec<Place> {
    local::places()
}

#[tauri::command]
async fn list_local(state: St<'_>, path: Option<String>) -> Result<Listing, String> {
    let hidden = state.settings.lock().unwrap().show_hidden;
    blocking(move || local::list(path.as_deref(), hidden)).await
}

/// Search the folder open on this computer and everything inside it.
#[tauri::command]
async fn search_local(window: tauri::Window, state: St<'_>, root: String, query: String, every_file: bool) -> Result<local::SearchResult, String> {
    let st = state.session(window.label());
    let counter = st.search_gen.clone();
    let generation = counter.fetch_add(1, Ordering::SeqCst) + 1;
    let hidden = st.settings.lock().unwrap().show_hidden;
    blocking(move || {
        let stale = || counter.load(Ordering::SeqCst) != generation;
        local::search(&root, &query, hidden, every_file, &stale)
    })
    .await
}

#[tauri::command]
fn search_cancel(window: tauri::Window, state: St) {
    state.session(window.label()).search_gen.fetch_add(1, Ordering::SeqCst);
}

#[tauri::command]
async fn measure_local(paths: Vec<String>) -> Result<Measure, String> {
    blocking(move || Ok(local::measure(&paths))).await
}

/// Move items on this computer to the Trash (where Finder can put them back). Refuses the top of a
/// disk, the home folder and anything holding it, and separate disks.
#[tauri::command]
async fn trash_local(paths: Vec<String>) -> Result<(), String> {
    blocking(move || {
        let home = archive_core::fsview::home();
        let mut items = Vec::new();
        for p in &paths {
            let path = PathBuf::from(p);
            if !path.is_absolute() {
                return Err(format!("{p} isn't a full path."));
            }
            if let Some(why) = archive_core::fsview::protected(&path, &home) {
                return Err(format!("Not moved to the Trash: {p} — {why}."));
            }
            if std::fs::symlink_metadata(&path).is_err() {
                return Err(format!("{p} isn't there any more."));
            }
            items.push(path);
        }
        #[cfg(target_os = "macos")]
        let result = {
            // The system call, which needs no permission from Finder.
            use trash::macos::{DeleteMethod, TrashContextExtMacos};
            let mut ctx = trash::TrashContext::default();
            ctx.set_delete_method(DeleteMethod::NsFileManager);
            ctx.delete_all(&items)
        };
        #[cfg(not(target_os = "macos"))]
        let result = trash::delete_all(&items);
        result.map_err(|e| format!("Couldn’t move it to the Trash: {e}"))
    })
    .await
}

/// Send items on a file server to its Trash (kept by the helper; see `archive_core::trash`).
/// Items the Trash couldn't be made for come back in `failed`, with whether deleting them is allowed.
#[tauri::command]
async fn trash_server(window: tauri::Window, state: St<'_>, server_id: String, paths: Vec<String>) -> Result<archive_core::trash::Report, String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    blocking(move || on_server(&st, &server, |r| r.fs_trash(&paths), |s| s.trash(&paths))).await
}

#[tauri::command]
async fn trash_list_server(window: tauri::Window, state: St<'_>, server_id: String) -> Result<Vec<archive_core::trash::Trashed>, String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    blocking(move || on_server(&st, &server, |r| r.fs_trash_list(), |s| s.trash_list())).await
}

#[tauri::command]
async fn trash_restore_server(window: tauri::Window, state: St<'_>, server_id: String, id: String) -> Result<String, String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    blocking(move || on_server(&st, &server, |r| r.fs_trash_restore(&id), |s| s.trash_restore(&id))).await
}

/// Delete items in a file server's Trash for good: the ones with these ids, or all of them.
#[tauri::command]
async fn trash_empty_server(window: tauri::Window, state: St<'_>, server_id: String, ids: Option<Vec<String>>) -> Result<usize, String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    blocking(move || on_server(&st, &server, |r| r.fs_trash_empty(ids.as_deref()), |s| s.trash_empty(ids.as_deref()))).await
}

/// Delete items on a file server immediately and for good (a file server has no Trash).
#[tauri::command]
async fn delete_server(window: tauri::Window, state: St<'_>, server_id: String, paths: Vec<String>) -> Result<usize, String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    blocking(move || on_server(&st, &server, |r| r.fs_delete(&paths), |s| s.delete(&paths))).await
}

/// "dir", "file", or "link" for each local path (items dropped in from Finder or Explorer).
#[tauri::command]
fn local_kinds(paths: Vec<String>) -> Vec<String> {
    paths
        .iter()
        .map(|p| match std::fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => "dir",
            Ok(m) if m.file_type().is_symlink() => "link",
            _ => "file",
        })
        .map(str::to_string)
        .collect()
}

#[tauri::command]
fn local_mkdir(path: String) -> Result<(), String> {
    std::fs::create_dir(&path).map_err(|e| format!("Couldn't create the folder: {e}"))
}

// ---------------------------------------------------------------------------
// Servers
// ---------------------------------------------------------------------------

#[tauri::command]
async fn connect(window: tauri::Window, state: St<'_>, server_id: String) -> Result<ConnInfo, String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    blocking(move || {
        let info = st.conns.connect(&st.ssh, &server);
        // Show this file server's transfer jobs, restarting any that were interrupted
        // (the server restarts its own; this computer restarts the ones it relays).
        if info.connected && info.needs.is_none() && server.kind == settings::ServerKind::Files {
            if let Ok(list) = st.conns.with(&st.ssh, &server, |r| r.job_list(true)) {
                st.jobs.adopt(&server, &list);
                for job in list.iter().filter(|j| j.state == "interrupted") {
                    if let Some(archive) = job.relay.as_deref().and_then(|a| st.server(a).ok()) {
                        st.jobs.relay(&server, &archive, &job.id);
                    }
                }
            }
        }
        Ok(info)
    })
    .await
}

// ---------------------------------------------------------------------------
// File servers
// ---------------------------------------------------------------------------

/// A file server's home folder and mounted drives, for its Places menu.
#[tauri::command]
async fn places_server(window: tauri::Window, state: St<'_>, server_id: String) -> Result<Vec<Place>, String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    blocking(move || on_server(&st, &server, |r| r.fs_places(), |s| Ok(s.places()))).await
}

#[tauri::command]
async fn list_server(window: tauri::Window, state: St<'_>, server_id: String, path: Option<String>) -> Result<Listing, String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    let hidden = st.settings.lock().unwrap().show_hidden;
    blocking(move || on_server(&st, &server, |r| r.fs_list(path.as_deref(), hidden), |s| s.list(path.as_deref(), hidden))).await
}

#[tauri::command]
async fn search_server(window: tauri::Window, 
    state: St<'_>,
    server_id: String,
    root: String,
    query: String,
    every_file: bool,
) -> Result<local::SearchResult, String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    let hidden = st.settings.lock().unwrap().show_hidden;
    blocking(move || on_server(&st, &server, |r| r.fs_search(&root, &query, every_file, hidden), |s| s.search(&root, &query, hidden, 2000, &|| false))).await
}

#[tauri::command]
async fn measure_server(window: tauri::Window, state: St<'_>, server_id: String, paths: Vec<String>) -> Result<Measure, String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    blocking(move || on_server(&st, &server, |r| r.fs_measure(&paths), |s| s.measure(&paths))).await
}

#[tauri::command]
async fn mkdir_server(window: tauri::Window, state: St<'_>, server_id: String, path: String) -> Result<(), String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    blocking(move || on_server(&st, &server, |r| r.fs_mkdir(&path), |s| s.mkdir(&path))).await
}

#[derive(Serialize)]
struct RouteStatus {
    /// The file server and the archive are the same machine: no route needed.
    same_machine: bool,
    /// A limited key has been set up for this file server.
    ready: bool,
    /// The archive lets file servers use limited keys (otherwise they sign in).
    keys: bool,
}

fn same_machine(st: &Session, files: &Server, archive: &Server) -> Result<bool, String> {
    let a = st.conns.host_of(&st.ssh, files).map_err(err)?;
    let b = st.conns.host_of(&st.ssh, archive).map_err(err)?;
    Ok(!a.is_empty() && a == b)
}

fn key_label(files: &Server) -> String {
    let name: String = files.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '-' }).collect();
    format!("{name}-{}", files.id)
}

#[tauri::command]
async fn route_status(window: tauri::Window, state: St<'_>, files_id: String, archive_id: String) -> Result<RouteStatus, String> {
    let st = state.session(window.label());
    let (files, archive) = (st.server(&files_id)?, st.server(&archive_id)?);
    blocking(move || {
        let same = same_machine(&st, &files, &archive)?;
        let route = settings::Route { files: files_id, archive: archive_id };
        let ready = st.settings.lock().unwrap().routes.contains(&route);
        Ok(RouteStatus { same_machine: same, ready, keys: archive.allow_keys })
    })
    .await
}

/// Let a file server reach an archive directly: give it a key the archive
/// accepts only for adding and reading data, and check that it works.
#[tauri::command]
async fn route_setup(window: tauri::Window, state: St<'_>, files_id: String, archive_id: String) -> Result<(), String> {
    let st = state.session(window.label());
    let (files, archive) = (st.server(&files_id)?, st.server(&archive_id)?);
    blocking(move || {
        let (c, ssh) = (&st.conns, &st.ssh);
        let key = c.with(ssh, &files, |r| r.transfer_key()).map_err(err)?;
        let label = key_label(&files);
        c.with(ssh, &archive, |r| r.key_authorize(&key, &label, None)).map_err(err)?;
        let (host, user, port) = ssh.resolve(&archive);
        let lines = ssh.known_host_lines(&host, port);
        if !lines.is_empty() {
            c.with(ssh, &files, |r| r.trust_hosts(&lines)).map_err(err)?;
        }
        let root = c.with(ssh, &archive, |r| Ok(r.hello().root.clone())).map_err(err)?;
        let target = ArchiveTarget::Ssh { host: host.clone(), port, user, root, auth: SshAuth::Key };
        let hello = match c.with(ssh, &files, |r| r.route_test(&target)) {
            Ok(h) => h,
            Err(e) => {
                let _ = c.with(ssh, &archive, |r| r.key_revoke(&label));
                return Err(format!("{} couldn't connect to {host}: {e}", files.name));
            }
        };
        // Now that we know the address it connects from, only accept the key from there.
        if let Some(ip) = hello.client_ip.as_deref() {
            c.with(ssh, &archive, |r| r.key_authorize(&key, &label, Some(ip))).map_err(err)?;
        }
        let mut s = st.settings.lock().unwrap();
        let route = settings::Route { files: files_id, archive: archive_id };
        if !s.routes.contains(&route) {
            s.routes.push(route);
        }
        settings::save(&st.settings_path, &s).map_err(|e| e.to_string())
    })
    .await
}

/// Remove a file server's direct access to an archive.
#[tauri::command]
async fn route_remove(window: tauri::Window, state: St<'_>, files_id: String, archive_id: String) -> Result<(), String> {
    let st = state.session(window.label());
    let (files, archive) = (st.server(&files_id)?, st.server(&archive_id)?);
    blocking(move || {
        st.conns.with(&st.ssh, &archive, |r| r.key_revoke(&key_label(&files))).map_err(err)?;
        let mut s = st.settings.lock().unwrap();
        s.routes.retain(|r| !(r.files == files_id && r.archive == archive_id));
        settings::save(&st.settings_path, &s).map_err(|e| e.to_string())
    })
    .await
}

/// Have a file server sign in to an archive server (unless it already is),
/// showing the user whatever the sign-in asks.
fn sign_in(st: &Session, files: &Server, archive_name: &str, link: &LinkTarget) -> Result<(), String> {
    st.conns
        .with(&st.ssh, files, |r| {
            // ssh asks the same question again when an answer is wrong.
            let mut last = String::new();
            r.sign_in(link, &mut |q| {
                let again = q == last;
                last = q.to_string();
                st.ssh.ask(archive_name, Some(&files.name), q, again)
            })
        })
        .map_err(err)
}

/// The file server couldn't reach the archive server over the network.
fn unreachable(msg: &str) -> bool {
    ["can't reach", "can't find", "timed out", "No route to host", "Connection refused", "Network is unreachable", "Could not resolve"]
        .iter()
        .any(|m| msg.contains(m))
}

/// The window offers to relay through this computer when it sees this prefix.
const RELAY_OFFER: &str = "RELAY_OFFER|";

/// Start a transfer between a file server and an archive, run by the file server.
async fn start_server_job(st: Arc<Session>, req: TransferRequest, files_id: String) -> Result<String, String> {
    let (files, archive) = (st.server(&files_id)?, st.server(&req.server_id)?);
    blocking(move || {
        let (c, ssh) = (&st.conns, &st.ssh);
        let root = c.with(ssh, &archive, |r| Ok(r.hello().root.clone())).map_err(err)?;
        let offer = |e: String| if unreachable(&e) { format!("{RELAY_OFFER}{e}") } else { e };
        let target = if same_machine(&st, &files, &archive)? {
            ArchiveTarget::Local { root }
        } else if req.relay || files.relay {
            ArchiveTarget::Relay { archive: archive.id.clone(), root }
        } else {
            let (host, user, port) = ssh.resolve(&archive);
            let route = settings::Route { files: files.id.clone(), archive: archive.id.clone() };
            let keyed = archive.allow_keys && st.settings.lock().unwrap().routes.contains(&route);
            let auth = if keyed { SshAuth::Key } else { SshAuth::SignIn };
            let target = ArchiveTarget::Ssh { host: host.clone(), port, user, root, auth };
            if let Some(link) = target.link() {
                // The file server checks the archive server's identity against what this computer knows.
                let lines = ssh.known_host_lines(&host, port);
                if !lines.is_empty() {
                    c.with(ssh, &files, |r| r.trust_hosts(&lines)).map_err(err)?;
                }
                sign_in(&st, &files, &archive.name, &link).map_err(offer)?;
            }
            target
        };
        let relayed = matches!(target, ArchiveTarget::Relay { .. });
        if !relayed {
            // Reach the archive the way the job will, so a problem shows up now rather than in the queue.
            c.with(ssh, &files, |r| r.route_test(&target))
                .map_err(|e| offer(format!("{} couldn't reach {}: {}", files.name, archive.name, err(e))))?;
        }
        let title = if relayed {
            format!("{} ({}, through this computer)", jobs::title(&req, &archive), files.name)
        } else {
            format!("{} (on {})", jobs::title(&req, &archive), files.name)
        };
        let spec = archive_core::jobs::JobSpec {
            title: title.clone(),
            archive_name: archive.name.clone(),
            direction: req.direction.clone(),
            archive: target,
            sources: req.sources.clone(),
            dest: req.dest.clone(),
            mode: if req.direction == "send" { req.mode.clone() } else { "copy".into() },
            conflict: req.conflict.clone(),
            new_project: req.new_project.clone(),
        };
        let id = c.with(ssh, &files, |r| r.job_start(&spec)).map_err(err)?;
        let status = archive_core::jobs::JobStatus {
            id,
            title,
            direction: req.direction.clone(),
            state: "queued".into(),
            message: "Starting on the server".into(),
            started: archive_core::util::now_secs(),
            updated: archive_core::util::now_secs(),
            archive: archive.name.clone(),
            relay: relayed.then(|| archive.id.clone()),
            ..Default::default()
        };
        let view = st.jobs.add_remote(&files, &status);
        st.jobs.remember(&view, &req);
        if relayed {
            st.jobs.relay(&files, &archive, &status.id);
        }
        Ok(view)
    })
    .await
}

/// Check a server from the Settings form (it may not be saved yet).
#[tauri::command]
async fn test_server(window: tauri::Window, state: St<'_>, server: Server) -> Result<ConnInfo, String> {
    let st = state.session(window.label());
    blocking(move || {
        let probe = Server { id: format!("probe-{}", archive_core::util::random_hex(4)), ..server };
        let info = st.conns.connect(&st.ssh, &probe);
        st.conns.forget(&probe.id);
        Ok(info)
    })
    .await
}

#[tauri::command]
async fn install_helper(window: tauri::Window, state: St<'_>, server: Server) -> Result<String, String> {
    let st = state.session(window.label());
    blocking(move || {
        archive_core::remote::install_helper(&server.destination(), server.port, &st.ssh.setup(&server), &st.helpers_dir).map_err(err)
    })
    .await
}

#[tauri::command]
async fn create_archive(window: tauri::Window, state: St<'_>, server: Server) -> Result<(), String> {
    let st = state.session(window.label());
    blocking(move || {
        let mut r = archive_core::remote::Remote::ssh_with(&server.target(), &st.ssh.setup(&server)).map_err(err)?;
        r.init(InitOptions { pack_target_bytes: None, par2_redundancy_percent: None, trash_days: None }).map_err(err)?;
        st.conns.forget(&server.id);
        Ok(())
    })
    .await
}

fn item(parent: &VPath, e: &Entry) -> Item {
    let path = parent.join(&e.name).map(|p| p.to_string()).unwrap_or_default();
    let kind = match e.kind {
        NodeKind::Dir => "dir",
        NodeKind::File => "file",
        NodeKind::Symlink => "link",
    }
    .to_string();
    Item {
        name: e.name.clone(),
        path,
        kind,
        // Space used in the datahold; Get info has the original size too.
        size: (e.kind != NodeKind::Symlink).then_some(e.stored),
        files: (e.kind == NodeKind::Dir).then_some(e.files),
        mtime: e.mtime_ns.div_euclid(1_000_000_000),
        archived: Some(e.created),
        is_project: e.is_project,
        in_project: e.in_project,
    }
}

#[tauri::command]
async fn list_archive(window: tauri::Window, state: St<'_>, server_id: String, path: Option<String>) -> Result<Listing, String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    blocking(move || {
        let dir = VPath::parse(path.as_deref().unwrap_or("/")).map_err(err)?;
        let (entries, free, here) =
            st.conns.with(&st.ssh, &server, |r| Ok((r.list(&dir)?, r.hello().free_bytes, r.stat(&dir)?))).map_err(err)?;
        let mut items: Vec<Item> = entries.iter().map(|e| item(&dir, e)).collect();
        local::sort(&mut items);
        let mut crumbs = vec![Crumb { name: server.name.clone(), path: "/".into() }];
        let mut cur = VPath::root();
        for c in dir.components() {
            cur = cur.join(c).map_err(err)?;
            crumbs.push(Crumb { name: c.clone(), path: cur.to_string() });
        }
        let project = here.and_then(|e| e.project_path);
        Ok(Listing { path: dir.to_string(), parent: dir.parent().map(|p| p.to_string()), crumbs, items, free_bytes: free, project })
    })
    .await
}

#[tauri::command]
async fn archive_info(window: tauri::Window, state: St<'_>, server_id: String, path: String) -> Result<Info, String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    blocking(move || {
        let p = VPath::parse(&path).map_err(err)?;
        st.conns.with(&st.ssh, &server, |r| r.info(&p)).map_err(err)
    })
    .await
}

#[derive(Serialize)]
struct Hit {
    folder: String,
    item: Item,
}

#[tauri::command]
async fn archive_search(window: tauri::Window, state: St<'_>, server_id: String, query: String) -> Result<Vec<Hit>, String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    blocking(move || {
        let hits = st.conns.with(&st.ssh, &server, |r| r.search(&query, 200)).map_err(err)?;
        Ok(hits
            .into_iter()
            .map(|(p, e)| {
                let parent = p.parent().unwrap_or_default();
                Hit { folder: parent.to_string(), item: item(&parent, &e) }
            })
            .collect())
    })
    .await
}

#[tauri::command]
async fn archive_mkdir(window: tauri::Window, state: St<'_>, server_id: String, path: String) -> Result<(), String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    blocking(move || {
        let p = VPath::parse(&path).map_err(err)?;
        st.conns.with(&st.ssh, &server, |r| r.create_folder(&p)).map_err(err)
    })
    .await
}

#[tauri::command]
async fn archive_rename(window: tauri::Window, state: St<'_>, server_id: String, from: String, to: String) -> Result<(), String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    blocking(move || {
        let (a, b) = (VPath::parse(&from).map_err(err)?, VPath::parse(&to).map_err(err)?);
        st.conns.with(&st.ssh, &server, |r| r.rename(&a, &b)).map_err(err)
    })
    .await
}

#[tauri::command]
async fn archive_trash(window: tauri::Window, state: St<'_>, server_id: String, paths: Vec<String>) -> Result<usize, String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    blocking(move || {
        let ps: Vec<VPath> = paths.iter().map(|p| VPath::parse(p)).collect::<Result<_, _>>().map_err(err)?;
        st.conns
            .with(&st.ssh, &server, |r| {
                for p in &ps {
                    r.trash(p)?;
                }
                Ok(ps.len())
            })
            .map_err(err)
    })
    .await
}

#[derive(Serialize)]
struct TrashItem {
    id: i64,
    name: String,
    origin: String,
    kind: &'static str,
    size: u64,
    files: u64,
    trashed_at: i64,
}

#[tauri::command]
async fn archive_trash_list(window: tauri::Window, state: St<'_>, server_id: String) -> Result<Vec<TrashItem>, String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    blocking(move || {
        let items = st.conns.with(&st.ssh, &server, |r| r.trash_list()).map_err(err)?;
        Ok(items
            .into_iter()
            .map(|e| {
                let origin = e.trash_origin.clone().unwrap_or_default();
                TrashItem {
                    id: e.id,
                    name: origin.rsplit('/').next().unwrap_or("").to_string(),
                    origin,
                    kind: if e.kind == NodeKind::Dir { "dir" } else { "file" },
                    size: e.size,
                    files: e.files,
                    trashed_at: e.trashed_at.unwrap_or(0),
                }
            })
            .collect())
    })
    .await
}

#[tauri::command]
async fn archive_restore(window: tauri::Window, state: St<'_>, server_id: String, id: i64) -> Result<String, String> {
    let st = state.session(window.label());
    let server = st.server(&server_id)?;
    blocking(move || st.conns.with(&st.ssh, &server, |r| r.restore(id, None)).map(|p| p.to_string()).map_err(err)).await
}

// ---------------------------------------------------------------------------
// Transfers and prompts
// ---------------------------------------------------------------------------

#[tauri::command]
async fn transfer_start(window: tauri::Window, state: St<'_>, req: TransferRequest) -> Result<String, String> {
    // Any window working on the same items counts, not just this one.
    if let Some(name) = state.already_transferring(&req) {
        return Err(already_transferring(&name));
    }
    let st = state.session(window.label());
    if req.direction == "copy" {
        return start_copy(&st, req);
    }
    if req.files_id.as_deref().and_then(|id| st.server(id).ok()).is_some_and(|f| f.kind == settings::ServerKind::Sftp) {
        return Err("An SFTP server can be used in Transfer mode, but not for stowing yet.".into());
    }
    if let Some(files_id) = req.files_id.clone() {
        return start_server_job(st, req, files_id).await;
    }
    let server = st.server(&req.server_id)?;
    Ok(st.jobs.submit(req, server, None))
}

/// A plain copy or move between this computer and file servers (Transfer mode).
/// It runs here, on this computer, with the bytes passing through it.
fn start_copy(st: &Session, req: TransferRequest) -> Result<String, String> {
    let file_server = |id: &str| -> Result<settings::Server, String> {
        let server = st.server(id)?;
        if server.kind == settings::ServerKind::Archive {
            return Err(format!("“{}” is a datahold. Use Stow mode to send to it.", server.name));
        }
        Ok(server)
    };
    let from = req.files_id.as_deref().map(file_server).transpose()?;
    let to = req.to_id.as_deref().map(file_server).transpose()?;
    let (first, second) = match (from, to) {
        (Some(a), Some(b)) => (a, Some(b)),
        (Some(a), None) | (None, Some(a)) => (a, None),
        (None, None) => return Err("Choose a file server for at least one side.".into()),
    };
    Ok(st.jobs.submit(req, first, second))
}

/// Sign in again for a server job that's waiting because its connection closed.
#[tauri::command]
async fn job_sign_in(window: tauri::Window, state: St<'_>, id: String) -> Result<(), String> {
    let st = state.session(window.label());
    blocking(move || {
        let (files, link, archive) =
            st.jobs.waiting_for(&id).ok_or_else(|| "That transfer isn't waiting for a sign-in any more.".to_string())?;
        sign_in(&st, &files, &archive, &link)?;
        st.jobs.watch(&files);
        Ok(())
    })
    .await
}

fn already_transferring(name: &str) -> String {
    format!("“{name}” is already being transferred. Wait for that transfer to finish, or stop it first.")
}

/// Before the send dialog: whether these items are already being transferred.
#[tauri::command]
fn transfer_busy(state: St, req: TransferRequest) -> Option<String> {
    state.already_transferring(&req).map(|name| already_transferring(&name))
}

#[tauri::command]
async fn transfer_cancel(window: tauri::Window, state: St<'_>, id: String) -> Result<(), String> {
    let st = state.session(window.label());
    blocking(move || st.jobs.cancel(&id)).await
}

#[tauri::command]
async fn transfer_pause(window: tauri::Window, state: St<'_>, id: String) -> Result<(), String> {
    let st = state.session(window.label());
    blocking(move || st.jobs.pause(&id)).await
}

#[tauri::command]
async fn transfer_resume(window: tauri::Window, state: St<'_>, id: String) -> Result<(), String> {
    let st = state.session(window.label());
    blocking(move || resume(&st, &id)).await
}

/// Play a paused transfer. A relayed server job is relayed through this computer again.
fn resume(st: &Session, id: &str) -> Result<(), String> {
    if let Some((files, archive_id, job)) = st.jobs.resume(id)? {
        let archive = st.server(&archive_id)?;
        st.jobs.relay(&files, &archive, &job);
    }
    Ok(())
}

/// Pause everything in progress, or play everything paused. Returns the first problem, if any.
#[tauri::command]
async fn transfers_pause_all(window: tauri::Window, state: St<'_>, pause: bool) -> Result<(), String> {
    let st = state.session(window.label());
    blocking(move || {
        let states: &[&str] = if pause { &["queued", "running", "waiting"] } else { &["paused"] };
        let ids = st.jobs.ids_in(states);
        let mut first = None;
        for id in ids {
            let r = if pause { st.jobs.pause(&id) } else { resume(&st, &id) };
            if let Err(e) = r {
                first.get_or_insert(e);
            }
        }
        first.map_or(Ok(()), Err)
    })
    .await
}

/// Abandon ship on one transfer of this window: stop it, then remove what it created, as if it
/// never started. A send's new barrels go to the datahold's trash (restorable for 30 days); a
/// retrieve's or a copy's new folders and files are deleted from where it put them. Only things the
/// transfer itself created are touched, and originals are never deleted before a transfer
/// completes, so nothing else is lost.
fn abandon_transfer(st: &Session, id: &str) -> Result<(), String> {
    st.jobs.cancel(&id)?;
    st.jobs.wait_stopped(&id, std::time::Duration::from_secs(120))?;
    let plan = st.jobs.abandon_plan(&id)?;
    let names = plan.created.iter().map(|p| format!("“{}”", p.trim_end_matches('/').rsplit('/').next().unwrap_or(p))).collect::<Vec<_>>();
    let message = if plan.direction == "copy" {
        if plan.created.is_empty() {
            "Stopped. It hadn’t created anything that needed removing.".into()
        } else {
            let (dest, server) = plan.copy_dest.clone().ok_or("That copy has no destination.")?;
            for p in plan.created.iter().filter(|p| jobs::is_directly_in(p, &dest)) {
                match &server {
                    Some(s) => on_server(st, s, |r| archive_core::copy::remove_tree(r, p), |x| archive_core::copy::remove_tree(x, p))?,
                    None => archive_core::copy::remove_tree(&mut archive_core::copy::LocalEnd::new(), p).map_err(err)?,
                }
            }
            format!("Abandoned: {} {} removed. Nothing was removed from where it came from.", names.join(", "), if names.len() == 1 { "was" } else { "were" })
        }
    } else if plan.direction == "send" {
        if !plan.created.is_empty() {
            let archive = match &plan.archive_id {
                Some(id) => st.server(id)?,
                None => {
                    let settings = st.settings.lock().unwrap();
                    let found = settings.servers.iter().find(|s| s.kind == settings::ServerKind::Archive && s.name == plan.archive_name).cloned();
                    found.ok_or_else(|| format!("Couldn't find the datahold “{}” in Settings.", plan.archive_name))?
                }
            };
            st.conns
                .with(&st.ssh, &archive, |r| {
                    for p in &plan.created {
                        let vp = VPath::parse(p)?;
                        if r.stat(&vp)?.is_some() {
                            r.trash(&vp)?;
                        }
                    }
                    Ok(())
                })
                .map_err(err)?;
            format!(
                "Abandoned: {} went overboard, where it can be brought back for 30 days. Nothing was removed from where it came from.",
                names.join(", ")
            )
        } else {
            "Stopped. It hadn’t started a barrel of its own, so there was nothing to throw overboard.".into()
        }
    } else {
        match (&plan.files, &plan.local_dest) {
            (Some((files, job)), _) => st.conns.with(&st.ssh, files, |r| r.job_discard(job)).map_err(err)?,
            (None, Some(dest)) => {
                for p in &plan.created {
                    let path = std::path::Path::new(p);
                    // Only what this transfer put in its destination folder.
                    if path.parent() != Some(std::path::Path::new(dest)) {
                        continue;
                    }
                    let removed = match std::fs::symlink_metadata(path) {
                        Ok(md) if md.is_dir() => std::fs::remove_dir_all(path),
                        Ok(_) => std::fs::remove_file(path),
                        Err(_) => Ok(()),
                    };
                    removed.map_err(|e| format!("Couldn't remove {p}: {e}"))?;
                }
            }
            (None, None) => {}
        }
        if plan.created.is_empty() {
            "Stopped. It hadn’t created anything that needed removing.".into()
        } else {
            format!("Abandoned: {} {} removed.", names.join(", "), if names.len() == 1 { "was" } else { "were" })
        }
    };
    st.jobs.abandoned(&id, message);
    Ok(())
}

#[tauri::command]
async fn transfer_abandon(window: tauri::Window, state: St<'_>, id: String) -> Result<(), String> {
    let st = state.session(window.label());
    blocking(move || abandon_transfer(&st, &id)).await
}

/// Close a finished transfer's row.
#[tauri::command]
fn transfer_dismiss(window: tauri::Window, state: St, id: String) {
    state.session(window.label()).jobs.dismiss(&id);
}

#[tauri::command]
fn transfers(window: tauri::Window, state: St) -> Vec<JobView> {
    state.session(window.label()).jobs.list()
}

#[tauri::command]
fn transfers_clear(window: tauri::Window, state: St) {
    state.session(window.label()).jobs.clear_finished();
}

#[tauri::command]
fn auth_answer(state: St, id: u64, answer: Option<String>) {
    state.askpass.answer(id, answer);
}

/// Wind down a window's transfers from this computer: "abandon" undoes the ones that are moving
/// (abandon ship), "pause" leaves them paused. Either way whatever is left is paused, for another
/// window to adopt.
fn wind_down(st: &Session, action: &str) -> Result<(), String> {
    if action == "abandon" {
        for id in st.jobs.active_local_ids() {
            abandon_transfer(st, &id)?;
        }
    }
    st.jobs.shutdown();
    Ok(())
}

/// The window's answer to "closing will interrupt these transfers": "pause" (they wait, to be
/// picked up by another window) or "abandon" (abandon ship: they are undone). Then it closes.
#[tauri::command]
async fn window_close(window: tauri::Window, state: St<'_>, action: String) -> Result<(), String> {
    let st = state.session(window.label());
    blocking(move || wind_down(&st, &action)).await?;
    window.destroy().map_err(|e| e.to_string())
}

/// The window's answer to "quitting will interrupt these transfers", for every window. Then it quits.
#[tauri::command]
async fn app_quit(app: tauri::AppHandle, state: St<'_>, action: String) -> Result<(), String> {
    let sessions = state.all_sessions();
    blocking(move || sessions.iter().try_for_each(|se| wind_down(se, &action))).await?;
    app.exit(0);
    Ok(())
}

/// Quit, asking first if transfers from this computer are moving.
fn request_quit(app: &tauri::AppHandle) {
    let registry = app.state::<Arc<Registry>>().inner().clone();
    match registry.leaving_app() {
        None => app.exit(0),
        Some(leaving) => {
            // Ask in the window in front, or any window there is.
            let windows = app.webview_windows();
            let ask = windows.values().find(|w| w.is_focused().unwrap_or(false)).or_else(|| windows.values().next());
            match ask {
                Some(w) => {
                    let _ = w.show();
                    let _ = w.set_focus();
                    let _ = app.emit_to(w.label(), "quit-requested", leaving);
                }
                None => app.exit(0),
            }
        }
    }
}

fn main() {
    // When ssh runs this program to ask for a password, answer and exit.
    askpass::run_if_askpass();

    tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();
            let config_dir = app.path().app_config_dir()?;
            let settings_path = config_dir.join("settings.json");
            let settings = settings::load(&settings_path);
            let askpass = askpass::Askpass::start(handle.clone())?;
            let bundled = app.path().resource_dir().map(|d| d.join("helpers")).ok().filter(|d| d.is_dir());
            let helpers_dir = bundled.unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dist/helpers")));
            app.manage(Arc::new(Registry {
                app: handle,
                settings_path,
                settings: Arc::new(Mutex::new(settings)),
                helpers_dir,
                askpass,
                saved: Arc::new(jobs::Saved::load(config_dir.join("transfers.json"))),
                next_transfer: Arc::new(AtomicU64::new(1)),
                sessions: Mutex::default(),
            }));
            Ok(())
        })
        .on_window_event(|window, event| {
            let registry = window.state::<Arc<Registry>>().inner().clone();
            match event {
                // With a transfer from this computer moving, closing asks first: the window shows
                // what would be interrupted and answers with `window_close`.
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    if let Some(leaving) = registry.leaving(window.label()) {
                        api.prevent_close();
                        let _ = window.app_handle().emit_to(window.label(), "close-requested", leaving);
                    }
                }
                // A closed window takes its connections and transfers with it; by now they
                // are paused or stopped, and whatever is paused is left for another window.
                tauri::WindowEvent::Destroyed => {
                    let label = window.label().to_string();
                    std::thread::spawn(move || registry.close_session(&label));
                }
                _ => {}
            }
        })
        .menu(|handle| {
            // The standard menu, with File ▸ New Window (⌘N) added.
            let menu = Menu::default(handle)?;
            let new_window = MenuItemBuilder::with_id("new-window", "New Window").accelerator("CmdOrCtrl+N").build(handle)?;
            let quit = MenuItemBuilder::with_id("quit", format!("Quit {}", handle.package_info().name)).accelerator("CmdOrCtrl+Q").build(handle)?;
            for item in menu.items()? {
                if let MenuItemKind::Submenu(sub) = item {
                    if sub.text()? == "File" {
                        sub.insert_items(&[&new_window, &PredefinedMenuItem::separator(handle)?], 0)?;
                    }
                    // The standard Quit leaves the program at once; this one asks first.
                    for entry in sub.items()? {
                        if let MenuItemKind::Predefined(p) = &entry {
                            if p.text()?.starts_with("Quit") {
                                sub.remove(p)?;
                                sub.append(&quit)?;
                            }
                        }
                    }
                }
            }
            Ok(menu)
        })
        .on_menu_event(|app, event| {
            match event.id().as_ref() {
                "new-window" => {
                    let _ = new_window(app);
                }
                "quit" => request_quit(app),
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            settings_get,
            settings_save,
            places,
            list_local,
            search_local,
            search_cancel,
            measure_local,
            local_kinds,
            local_mkdir,
            connect,
            list_server,
            places_server,
            search_server,
            measure_server,
            mkdir_server,
            delete_server,
            trash_server,
            trash_list_server,
            trash_restore_server,
            trash_empty_server,
            trash_local,
            route_status,
            route_setup,
            route_remove,
            test_server,
            install_helper,
            create_archive,
            list_archive,
            archive_info,
            archive_search,
            archive_mkdir,
            archive_rename,
            archive_trash,
            archive_trash_list,
            archive_restore,
            transfer_start,
            transfer_cancel,
            transfer_pause,
            transfer_resume,
            transfer_dismiss,
            transfer_abandon,
            transfers_pause_all,
            transfer_busy,
            job_sign_in,
            transfers,
            transfers_clear,
            auth_answer,
            window_close,
            app_quit,
        ])
        .build(tauri::generate_context!())
        .expect("error while building the Archive app")
        .run(|app, event| match event {
            // On a Mac, closing the last window leaves the app running (transfers carry on,
            // and its Dock icon opens a window again). Quit, ⌘Q, still quits.
            tauri::RunEvent::ExitRequested { api, code, .. } if cfg!(target_os = "macos") && code.is_none() => api.prevent_exit(),
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { has_visible_windows: false, .. } => {
                let _ = new_window(app);
            }
            _ => {}
        });
}

/// Open another window, just below and to the right of the one in front. Each has its own
/// panes and mode; transfers, servers, and settings are shared.
fn new_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    static NEXT: AtomicU32 = AtomicU32::new(2);
    let front = app.webview_windows().into_values().find(|w| w.is_focused().unwrap_or(false));
    let mut config = app.config().app.windows.first().cloned().ok_or(tauri::Error::WindowNotFound)?;
    config.label = format!("w{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let window = tauri::WebviewWindowBuilder::from_config(app, &config)?.build()?;
    if let Some(front) = front {
        if let (Ok(at), Ok(scale)) = (front.outer_position(), front.scale_factor()) {
            let step = (28.0 * scale) as i32;
            let _ = window.set_position(tauri::PhysicalPosition::new(at.x + step, at.y + step));
        }
    }
    Ok(())
}
