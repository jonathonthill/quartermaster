//! Transfer jobs that run on a server (a file server sending to or retrieving
//! from an archive), independent of the app that started them.
//!
//! Each job has a folder under `~/.local/share/archive-helper/jobs/<id>/`:
//! `spec.json` (what to do), `status.json` (progress, rewritten about once a
//! second), `lock` (held while the job runs), and `cancel` (created to stop it).
//! A job whose status says "running" but whose lock is free was interrupted
//! (for example by a reboot) and can be started again; files it already
//! archived are recognized and skipped.
//!
//! A job that reaches the archive over a signed-in link (see [`crate::link`])
//! waits, in the "waiting" state, whenever the link is closed, and carries on
//! from where it stopped once the user signs in again.
//!
//! A relayed job reaches the archive through the computer that started it:
//! that computer runs `archive-helper job run` over SSH and joins the job's
//! stdin and stdout to an archive session of its own. Only that computer can
//! (re)start such a job, so the server never restarts one by itself.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::api::{Archive, Conflict};
use crate::error::{Error, Result};
use crate::link::{self, LinkTarget};
use crate::remote::{Remote, SshSetup, SshTarget};
use crate::retrieve::{self, LocalConflict, RetrieveOptions};
use crate::send::{self, Mode, SendOptions};
use crate::store::Store;
use crate::util::{self, human_bytes, now_secs};
use crate::vpath::VPath;

/// How a job reaches the archive.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "via", rename_all = "snake_case")]
pub enum ArchiveTarget {
    /// The archive is on this same machine: write to it directly.
    Local { root: String },
    /// Over SSH, signing in as `auth` says.
    Ssh {
        host: String,
        port: Option<u16>,
        user: Option<String>,
        root: String,
        #[serde(default)]
        auth: SshAuth,
    },
    /// Through the computer that started the job, over the job's own stdin
    /// and stdout. `archive` identifies the archive to that computer.
    Relay { archive: String, root: String },
}

/// How a job on a file server signs in to the archive server.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SshAuth {
    /// This server's limited transfer key (see [`crate::keys`]).
    #[default]
    Key,
    /// A connection the user signed in to from the app (see [`crate::link`]).
    SignIn,
}

impl ArchiveTarget {
    /// The signed-in link this target uses, if any.
    pub fn link(&self) -> Option<LinkTarget> {
        match self {
            ArchiveTarget::Ssh { host, port, user, auth: SshAuth::SignIn, .. } => {
                Some(LinkTarget { host: host.clone(), port: *port, user: user.clone() })
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobSpec {
    pub title: String,
    /// The archive's name in the app, for messages.
    #[serde(default)]
    pub archive_name: String,
    /// "send" (this server → archive) or "retrieve" (archive → this server).
    pub direction: String,
    pub archive: ArchiveTarget,
    /// Paths on this server (send) or in the archive (retrieve).
    pub sources: Vec<String>,
    /// Archive folder (send) or folder on this server (retrieve).
    pub dest: String,
    /// "copy" or "move" (sending only).
    pub mode: String,
    /// "skip" or "keep-both" ("replace" is refused: archived projects are frozen).
    pub conflict: String,
    /// Sending: put everything in a new project folder of this name in `dest`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_project: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct JobStatus {
    pub id: String,
    pub title: String,
    pub direction: String,
    /// "queued", "running", "waiting" (for the user to sign in again),
    /// "paused", "done", "failed", "cancelled", or "interrupted".
    pub state: String,
    pub done: u64,
    pub total: u64,
    pub current: String,
    pub message: String,
    pub problems: Vec<String>,
    pub started: i64,
    pub updated: i64,
    /// Bytes per second so far.
    pub rate: u64,
    /// The archive's name in the app.
    #[serde(default)]
    pub archive: String,
    /// While waiting: the archive server to sign in to again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<LinkTarget>,
    /// For a relayed job: the archive, as the computer relaying it knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    /// What this job created that wasn't there before, so abandoning it can
    /// remove exactly that: project folders in the archive (sending), or
    /// folders and files on this server (retrieving).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created: Vec<String>,
}

pub fn jobs_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp")).join(".local/share/archive-helper/jobs")
}

fn job_dir(base: &Path, id: &str) -> Result<PathBuf> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(Error::InvalidPath("bad job id".into()));
    }
    Ok(base.join(id))
}

fn write_status(dir: &Path, st: &JobStatus) -> Result<()> {
    util::atomic_write(&dir.join("status.json"), &serde_json::to_vec_pretty(st)?)?;
    Ok(())
}

fn read_status(dir: &Path) -> Option<JobStatus> {
    serde_json::from_slice(&fs::read(dir.join("status.json")).ok()?).ok()
}

/// Is the job's process alive? (Its lock is held while it runs.)
fn running(dir: &Path) -> bool {
    match OpenOptions::new().write(true).open(dir.join("lock")) {
        Ok(f) => !util::try_lock(&f).unwrap_or(true),
        Err(_) => false,
    }
}

/// Record a new job. Start it with `run` (the helper does so in the background).
pub fn create(spec: &JobSpec) -> Result<String> {
    create_in(&jobs_dir(), spec)
}

pub fn create_in(base: &Path, spec: &JobSpec) -> Result<String> {
    let id = format!("{}-{}", now_secs(), util::random_hex(3));
    let dir = job_dir(base, &id)?;
    fs::create_dir_all(&dir)?;
    util::atomic_write(&dir.join("spec.json"), &serde_json::to_vec_pretty(spec)?)?;
    let st = JobStatus {
        id: id.clone(),
        title: spec.title.clone(),
        direction: spec.direction.clone(),
        state: "queued".into(),
        message: "Starting".into(),
        archive: spec.archive_name.clone(),
        relay: match &spec.archive {
            ArchiveTarget::Relay { archive, .. } => Some(archive.clone()),
            _ => None,
        },
        started: now_secs(),
        updated: now_secs(),
        ..Default::default()
    };
    write_status(&dir, &st)?;
    Ok(id)
}

pub fn cancel(id: &str) -> Result<()> {
    cancel_in(&jobs_dir(), id)
}

pub fn cancel_in(base: &Path, id: &str) -> Result<()> {
    let dir = job_dir(base, id)?;
    fs::write(dir.join("cancel"), b"")?;
    // A job that isn't running (paused, interrupted, or not started yet) won't
    // see the request, so record it as stopped now.
    if let Some(mut st) = read_status(&dir).filter(|s| matches!(s.state.as_str(), "paused" | "interrupted" | "queued")) {
        if !running(&dir) {
            st.state = "cancelled".into();
            st.message = "Stopped. Files archived before it stopped are kept.".into();
            st.link = None;
            st.updated = now_secs();
            write_status(&dir, &st)?;
        }
    }
    Ok(())
}

/// Ask a job to pause: it stops at the next file and waits for `resume`.
pub fn pause(id: &str) -> Result<()> {
    pause_in(&jobs_dir(), id)
}

pub fn pause_in(base: &Path, id: &str) -> Result<()> {
    let dir = job_dir(base, id)?;
    fs::write(dir.join("pause"), b"")?;
    if let Some(mut st) = read_status(&dir).filter(|s| matches!(s.state.as_str(), "interrupted" | "queued")) {
        if !running(&dir) {
            paused(&mut st);
            write_status(&dir, &st)?;
        }
    }
    Ok(())
}

/// Let a paused (or interrupted) job continue. Returns whether this server
/// should start it; a relayed job is started by the computer relaying it.
pub fn resume(id: &str) -> Result<bool> {
    resume_in(&jobs_dir(), id)
}

pub fn resume_in(base: &Path, id: &str) -> Result<bool> {
    let dir = job_dir(base, id)?;
    let _ = fs::remove_file(dir.join("pause"));
    let Some(mut st) = read_status(&dir) else { return Err(Error::other("that transfer isn't on this server any more")) };
    if !matches!(st.state.as_str(), "paused" | "interrupted") || running(&dir) {
        return Ok(false);
    }
    st.state = "queued".into();
    st.message = "Continuing where it stopped".into();
    st.updated = now_secs();
    write_status(&dir, &st)?;
    Ok(st.relay.is_none())
}

fn paused(st: &mut JobStatus) {
    st.state = "paused".into();
    st.message = "Paused. Press play to continue where it stopped; files already archived are kept.".into();
    st.rate = 0;
    st.link = None;
    st.updated = now_secs();
}

/// Abandon a stopped retrieve: delete what it created on this server, so it's
/// as if it never ran. (A send's project folders are in the archive; the app
/// removes those.)
pub fn discard(id: &str) -> Result<()> {
    discard_in(&jobs_dir(), id)
}

pub fn discard_in(base: &Path, id: &str) -> Result<()> {
    let dir = job_dir(base, id)?;
    let mut st = read_status(&dir).ok_or_else(|| Error::other("that transfer isn't on this server any more"))?;
    if running(&dir) || matches!(st.state.as_str(), "queued" | "running" | "waiting") {
        return Err(Error::other("stop the transfer first"));
    }
    if st.direction != "send" {
        let spec: JobSpec = serde_json::from_slice(&fs::read(dir.join("spec.json"))?)?;
        for p in &st.created {
            let path = Path::new(p);
            // Only what this job put in its destination folder.
            if path.parent() != Some(Path::new(&spec.dest)) {
                continue;
            }
            let removed = match fs::symlink_metadata(path) {
                Ok(md) if md.is_dir() => fs::remove_dir_all(path),
                Ok(_) => fs::remove_file(path),
                Err(_) => Ok(()),
            };
            removed.map_err(|e| Error::other(format!("couldn't remove {p}: {e}")))?;
        }
        st.message = "Abandoned: what it had retrieved was removed.".into();
        st.created.clear();
    }
    st.state = "cancelled".into();
    st.updated = now_secs();
    write_status(&dir, &st)
}

/// Whether the user asked this job to stop or pause.
fn stop_requested(dir: &Path) -> bool {
    dir.join("cancel").exists() || dir.join("pause").exists()
}

/// All jobs, newest first. Finished jobs older than 30 days are cleaned up.
pub fn list() -> Vec<JobStatus> {
    list_in(&jobs_dir())
}

pub fn list_in(base: &Path) -> Vec<JobStatus> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(base) else { return out };
    for e in rd.flatten() {
        let dir = e.path();
        let Some(mut st) = read_status(&dir) else { continue };
        let finished = matches!(st.state.as_str(), "done" | "failed" | "cancelled");
        if finished && now_secs() - st.updated > 30 * 86_400 {
            let _ = fs::remove_dir_all(&dir);
            continue;
        }
        if matches!(st.state.as_str(), "running" | "queued" | "waiting") && !running(&dir) && now_secs() - st.updated > 15 {
            st.state = "interrupted".into();
            st.message =
                "Stopped before it finished (the server may have restarted). Start it again to continue; finished files are kept.".into();
        }
        out.push(st);
    }
    out.sort_by(|a, b| b.id.cmp(&a.id));
    out
}

/// Jobs that were interrupted and that this server can restart by itself
/// (relayed jobs need the computer that relays them).
pub fn interrupted() -> Vec<String> {
    list_in(&jobs_dir()).into_iter().filter(|s| s.state == "interrupted" && s.relay.is_none()).map(|s| s.id).collect()
}

/// Whether a job should be started here in the background when it's created.
pub fn starts_by_itself(spec: &JobSpec) -> bool {
    !matches!(spec.archive, ArchiveTarget::Relay { .. })
}

/// Connect to an archive the way a job does, and describe it.
pub fn route_test(target: &ArchiveTarget) -> Result<crate::proto::HelloInfo> {
    match target {
        ArchiveTarget::Local { root } => {
            let store = Store::open(Path::new(root))?;
            Ok(crate::proto::HelloInfo::here(root.clone(), Some(store.config().archive_id.clone()), false, String::new()))
        }
        ArchiveTarget::Ssh { .. } => Ok(connect_ssh(target)?.hello().clone()),
        ArchiveTarget::Relay { .. } => Err(Error::other("a relayed route can only be used by a running job")),
    }
}

/// Connect to an archive over SSH with the target's kind of sign-in.
fn connect_ssh(target: &ArchiveTarget) -> Result<Remote> {
    let ArchiveTarget::Ssh { host, port, user, root, auth } = target else { unreachable!() };
    let t = ssh_target(host, *port, user.as_deref(), root);
    let args = match auth {
        SshAuth::Key => crate::keys::transfer_ssh_args(),
        SshAuth::SignIn => {
            let l = target.link().unwrap();
            if !link::check(&l) {
                return Err(Error::SignedOut(l.label()));
            }
            link::session_args(&l)?
        }
    };
    Remote::ssh_with(&t, &SshSetup { args, env: Vec::new() })
}

fn ssh_target(host: &str, port: Option<u16>, user: Option<&str>, root: &str) -> SshTarget {
    SshTarget {
        host: match user {
            Some(u) if !u.is_empty() => format!("{u}@{host}"),
            _ => host.to_string(),
        },
        port,
        root: root.to_string(),
        helper: crate::remote::DEFAULT_HELPER.to_string(),
        files: false,
    }
}

fn open_archive(target: &ArchiveTarget) -> Result<Box<dyn Archive>> {
    match target {
        ArchiveTarget::Local { root } => Ok(Box::new(Store::open(Path::new(root))?)),
        ArchiveTarget::Ssh { .. } => Ok(Box::new(connect_ssh(target)?)),
        // The relaying computer joined this process's stdin and stdout to the archive.
        ArchiveTarget::Relay { .. } => Ok(Box::new(Remote::connect(Box::new(std::io::stdin()), Box::new(std::io::stdout()))?)),
    }
}

/// Run a job to completion in this process (the helper runs it detached).
pub fn run(id: &str) -> Result<()> {
    run_in(&jobs_dir(), id)
}

pub fn run_in(base: &Path, id: &str) -> Result<()> {
    let dir = job_dir(base, id)?;
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("lock"))?;
    if !util::try_lock(&lock)? {
        return Err(Error::other("this job is already running"));
    }
    let spec: JobSpec = serde_json::from_slice(&fs::read(dir.join("spec.json"))?)?;
    let mut st = read_status(&dir).unwrap_or_default();
    st.id = id.to_string();
    st.state = "running".into();
    st.message = "Connecting to the archive".into();
    st.updated = now_secs();
    write_status(&dir, &st)?;

    let cancel = Arc::new(AtomicBool::new(false));
    let pause_requested = || dir.join("pause").exists() && !dir.join("cancel").exists();
    if pause_requested() {
        paused(&mut st);
        return write_status(&dir, &st);
    }
    // A stop requested before the job (re)started still counts.
    let result = if dir.join("cancel").exists() {
        cancel.store(true, Ordering::Relaxed);
        Ok(("Nothing more was transferred.".into(), Vec::new(), true))
    } else {
        run_attempts(&spec, id, &dir, &mut st, &cancel)
    };

    // Paused partway: keep its progress, and wait to be resumed.
    if pause_requested() {
        paused(&mut st);
        return write_status(&dir, &st);
    }
    st.updated = now_secs();
    st.link = None;
    match result {
        Ok((msg, problems, bad)) => {
            let cancelled = cancel.load(Ordering::Relaxed);
            st.state = if cancelled {
                "cancelled".into()
            } else if bad {
                "failed".into()
            } else {
                "done".into()
            };
            st.message = if cancelled { format!("Stopped. {msg}") } else { msg };
            st.problems = problems;
            st.done = st.total;
        }
        Err(e) if e.is_lost() && matches!(spec.archive, ArchiveTarget::Relay { .. }) => {
            st.state = "interrupted".into();
            st.message = "Paused: this transfer goes through the computer that started it, which disconnected. \
                          It continues when the Archive app there reconnects; finished files are kept."
                .into();
        }
        Err(e) => {
            st.state = "failed".into();
            st.message = e.to_string();
        }
    }
    write_status(&dir, &st)?;
    Ok(())
}

/// Run the job, and over a signed-in link, keep going through lost
/// connections: wait for the user to sign in again, then continue.
fn run_attempts(spec: &JobSpec, id: &str, dir: &Path, st: &mut JobStatus, cancel: &Arc<AtomicBool>) -> Result<(String, Vec<String>, bool)> {
    let mut drops = 0;
    loop {
        let err = match attempt(spec, id, dir, st, cancel) {
            Err(e) if e.is_lost() || matches!(e, Error::SignedOut(_)) => e,
            other => return other,
        };
        let Some(l) = spec.archive.link() else { return Err(err) };
        if stop_requested(dir) {
            cancel.store(true, Ordering::Relaxed);
            return Ok(("Files archived before the connection closed are kept.".into(), Vec::new(), true));
        }
        if link::check(&l) {
            // The link is up, so this was a hiccup in one session. Retry, but not forever.
            drops += 1;
            if drops > 3 {
                return Err(err);
            }
            std::thread::sleep(Duration::from_secs(10));
            continue;
        }
        drops = 0;
        let host = l.host.clone();
        st.state = "waiting".into();
        st.message = format!(
            "Paused: the connection to {host} closed. Sign in again in the Archive app and this transfer continues where it stopped."
        );
        st.link = Some(l.clone());
        st.updated = now_secs();
        write_status(dir, st)?;
        match wait_for_link(&l, dir) {
            Wait::SignedIn => {}
            Wait::Stopped => {
                cancel.store(true, Ordering::Relaxed);
                st.link = None;
                return Ok(("Files archived before the connection closed are kept.".into(), Vec::new(), true));
            }
            Wait::GaveUp => {
                return Err(Error::other(format!("Nobody signed in to {host} again for {WAIT_DAYS} days, so this transfer stopped. Start it again to continue.")));
            }
        }
        st.state = "running".into();
        st.message = "Reconnecting to the archive".into();
        st.link = None;
        st.updated = now_secs();
        write_status(dir, st)?;
    }
}

/// How long a job waits for the user to sign in again.
const WAIT_DAYS: u64 = 14;

enum Wait {
    SignedIn,
    /// Cancelled or paused.
    Stopped,
    GaveUp,
}

fn wait_for_link(l: &LinkTarget, dir: &Path) -> Wait {
    let started = Instant::now();
    loop {
        if stop_requested(dir) {
            return Wait::Stopped;
        }
        if link::check(l) {
            return Wait::SignedIn;
        }
        if started.elapsed() > Duration::from_secs(WAIT_DAYS * 86_400) {
            return Wait::GaveUp;
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

fn attempt(spec: &JobSpec, id: &str, dir: &Path, st: &mut JobStatus, cancel: &Arc<AtomicBool>) -> Result<(String, Vec<String>, bool)> {
    let mut archive = open_archive(&spec.archive)?;
    let mut last = Instant::now() - Duration::from_secs(5);
    let mut current = String::new();
    let mut speed = crate::util::Speed::new(Duration::from_secs(10));
    let mut progress = |done: u64, total: u64, cur: &str, st: &mut JobStatus| {
        if stop_requested(dir) {
            cancel.store(true, Ordering::Relaxed);
        }
        if last.elapsed() < Duration::from_secs(1) {
            return;
        }
        last = Instant::now();
        st.done = done;
        st.total = total;
        st.current = cur.to_string();
        st.rate = speed.update(done);
        st.message = "Transferring".into();
        st.updated = now_secs();
        let _ = write_status(dir, st);
    };
    let policy = spec.conflict.as_str();
    if spec.direction == "send" {
        let opts = SendOptions {
            mode: if spec.mode == "move" { Mode::Move } else { Mode::Copy },
            policy: match policy {
                "replace" => Conflict::Replace,
                "keep-both" => Conflict::KeepBoth,
                _ => Conflict::Skip,
            },
            // The job id doubles as the resume key: a restart fills the same pack.
            job: id.to_string(),
            cancel: Some(cancel.clone()),
            new_project: spec.new_project.clone(),
            ..SendOptions::default()
        };
        let sources: Vec<PathBuf> = spec.sources.iter().map(PathBuf::from).collect();
        let dest = VPath::parse(&spec.dest)?;
        let r = send::send(archive.as_mut(), &sources, &dest, &opts, &mut |e| match e {
            send::Event::Scanned { bytes, .. } => progress(0, *bytes, "", st),
            send::Event::Sending { path } | send::Event::Checking { path } => {
                current = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            }
            send::Event::Verifying { path } => {
                current = format!("{} (being checked)", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default());
                progress(st.done, st.total, &current, st);
            }
            send::Event::Progress { done, total } => progress(*done, *total, &current, st),
            send::Event::Project { path, new: true } => {
                let p = path.to_string();
                if !st.created.contains(&p) {
                    st.created.push(p);
                    let _ = write_status(dir, st);
                }
            }
            _ => {}
        })?;
        drop(archive);
        start_local_maintenance(&spec.archive);
        let archived = r.stored + r.deduplicated + r.identical;
        let mut msg = format!("{archived} of {} files archived and verified", r.files);
        if r.originals_kept {
            msg.push_str("; the originals were kept, since not everything was archived");
        } else if opts.mode == Mode::Move {
            msg.push_str(&format!("; {} removed from this server", r.deleted));
        }
        if !r.skipped.is_empty() {
            msg.push_str(&format!("; {} skipped (a different item already has that name)", r.skipped.len()));
        }
        if !r.failed.is_empty() {
            msg.push_str(&format!("; {} failed", r.failed.len()));
        }
        msg.push_str(&format!(" ({} sent)", human_bytes(r.bytes_sent)));
        let problems = r
            .failed
            .iter()
            .map(|(p, e)| format!("{}: {e}", p.display()))
            .chain(r.kept.iter().map(|(p, e)| format!("Kept {}: {e}", p.display())))
            .take(200)
            .collect();
        Ok((msg, problems, r.cancelled || !r.failed.is_empty()))
    } else {
        let opts = RetrieveOptions {
            policy: match policy {
                "replace" => LocalConflict::Replace,
                "keep-both" => LocalConflict::KeepBoth,
                _ => LocalConflict::Skip,
            },
            cancel: Some(cancel.clone()),
            ..RetrieveOptions::default()
        };
        let dest = PathBuf::from(&spec.dest);
        let (mut files, mut written, mut skipped, mut problems, mut base) = (0u64, 0u64, 0usize, Vec::new(), 0u64);
        for src in &spec.sources {
            // Note what this creates, before it does (a resumed job keeps what it noted).
            if let Some(name) = src.trim_end_matches('/').rsplit('/').next().filter(|n| !n.is_empty()) {
                let target = dest.join(name);
                let p = target.display().to_string();
                if !target.exists() && !st.created.contains(&p) {
                    st.created.push(p);
                    write_status(dir, st)?;
                }
            }
            let r = retrieve::retrieve(archive.as_mut(), &VPath::parse(src)?, &dest, &opts, &mut |e| match e {
                retrieve::Event::Receiving { path } => {
                    current = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                }
                retrieve::Event::Progress { done, total } => progress(base + done, base + total, &current, st),
                _ => {}
            })?;
            base += r.bytes;
            files += r.files;
            written += r.written;
            skipped += r.skipped.len();
            problems.extend(r.failed.iter().map(|(p, e)| format!("{}: {e}", p.display())));
            if cancel.load(Ordering::Relaxed) {
                break;
            }
        }
        let mut msg = format!("{written} of {files} files retrieved and verified");
        if skipped > 0 {
            msg.push_str(&format!("; {skipped} already there (skipped)"));
        }
        if !problems.is_empty() {
            msg.push_str(&format!("; {} failed", problems.len()));
        }
        let bad = !problems.is_empty() || cancel.load(Ordering::Relaxed);
        problems.truncate(200);
        Ok((msg, problems, bad))
    }
}

/// After writing to an archive on this machine, start its background
/// maintenance. (Over SSH, the archive's own helper does this when the job's
/// pack is sealed.)
fn start_local_maintenance(target: &ArchiveTarget) {
    if let (ArchiveTarget::Local { root }, Ok(exe)) = (target, std::env::current_exe()) {
        let _ = util::spawn_detached(&exe, &["worker", "--root", root]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Config;

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join("jobs");
        let root = t.path().join("arc");
        let src = t.path().join("data/run42");
        fs::create_dir_all(src.join("sub")).unwrap();
        for i in 0..5 {
            fs::write(src.join(format!("reads_{i}.fastq")), format!("@read{i}\nACGT\n+\nFFFF\n").repeat(5000)).unwrap();
        }
        fs::write(src.join("sub/notes.txt"), b"pilot").unwrap();
        Store::init(&root, Config::default()).unwrap();
        (t, base, root, src)
    }

    fn spec(direction: &str, root: &Path, sources: Vec<String>, dest: &str, mode: &str) -> JobSpec {
        JobSpec {
            title: "test".into(),
            archive_name: "arc".into(),
            direction: direction.into(),
            archive: ArchiveTarget::Local { root: root.display().to_string() },
            sources,
            dest: dest.into(),
            mode: mode.into(),
            conflict: "skip".into(),
            new_project: None,
        }
    }

    #[test]
    fn send_then_retrieve_jobs() {
        let (t, base, root, src) = setup();
        let id = create_in(&base, &spec("send", &root, vec![src.display().to_string()], "/Projects", "move")).unwrap();
        assert_eq!(list_in(&base)[0].state, "queued");
        run_in(&base, &id).unwrap();
        let st = &list_in(&base)[0];
        assert_eq!(st.state, "done", "{st:?}");
        assert!(st.message.starts_with("6 of 6 files archived"), "{}", st.message);
        assert!(!src.exists(), "Move removes the source");

        let out = t.path().join("back");
        fs::create_dir_all(&out).unwrap();
        let id2 = create_in(&base, &spec("retrieve", &root, vec!["/Projects/run42".into()], out.to_str().unwrap(), "copy")).unwrap();
        run_in(&base, &id2).unwrap();
        let st2 = list_in(&base).into_iter().find(|s| s.id == id2).unwrap();
        assert_eq!(st2.state, "done", "{st2:?}");
        assert_eq!(fs::read_to_string(out.join("run42/sub/notes.txt")).unwrap(), "pilot");
        assert_eq!(fs::read_dir(out.join("run42")).unwrap().count(), 6);
    }

    #[test]
    fn cancelled_and_interrupted_jobs() {
        let (_t, base, root, src) = setup();
        let id = create_in(&base, &spec("send", &root, vec![src.display().to_string()], "/", "move")).unwrap();
        // Stopping a job that isn't running (yet) keeps it from running.
        cancel_in(&base, &id).unwrap();
        run_in(&base, &id).unwrap();
        let st = read_status(&base.join(&id)).unwrap();
        assert_eq!(st.state, "cancelled", "{st:?}");
        assert!(src.join("reads_0.fastq").exists(), "nothing moved");

        // A job that says it's running but holds no lock was interrupted.
        let id2 = create_in(&base, &spec("send", &root, vec![src.display().to_string()], "/", "copy")).unwrap();
        let d = base.join(&id2);
        let mut st = read_status(&d).unwrap();
        st.state = "running".into();
        st.updated = now_secs() - 60;
        write_status(&d, &st).unwrap();
        let listed = list_in(&base).into_iter().find(|s| s.id == id2).unwrap();
        assert_eq!(listed.state, "interrupted");
        // So is one that was waiting for a sign-in when its process ended.
        st.state = "waiting".into();
        write_status(&d, &st).unwrap();
        assert_eq!(list_in(&base).into_iter().find(|s| s.id == id2).unwrap().state, "interrupted");
        assert!(job_dir(&base, "../escape").is_err());
    }

    #[test]
    fn discarding_a_retrieve_removes_only_what_it_created() {
        let (t, base, root, src) = setup();
        let id = create_in(&base, &spec("send", &root, vec![src.display().to_string()], "/Projects", "copy")).unwrap();
        run_in(&base, &id).unwrap();
        let st = read_status(&base.join(&id)).unwrap();
        assert_eq!(st.created, vec!["/Projects/run42".to_string()], "the send created this project");

        // Retrieved into a fresh folder: abandoning removes it.
        let out = t.path().join("back");
        fs::create_dir_all(&out).unwrap();
        let id2 = create_in(&base, &spec("retrieve", &root, vec!["/Projects/run42".into()], out.to_str().unwrap(), "copy")).unwrap();
        run_in(&base, &id2).unwrap();
        assert!(out.join("run42/sub/notes.txt").exists());
        discard_in(&base, &id2).unwrap();
        assert!(!out.join("run42").exists());
        assert_eq!(read_status(&base.join(&id2)).unwrap().state, "cancelled");

        // Retrieved into a folder that was already there: abandoning leaves it alone.
        fs::create_dir_all(out.join("run42")).unwrap();
        fs::write(out.join("run42/mine.txt"), b"keep").unwrap();
        let id3 = create_in(&base, &spec("retrieve", &root, vec!["/Projects/run42".into()], out.to_str().unwrap(), "copy")).unwrap();
        run_in(&base, &id3).unwrap();
        discard_in(&base, &id3).unwrap();
        assert_eq!(fs::read_to_string(out.join("run42/mine.txt")).unwrap(), "keep");
    }

    #[test]
    fn paused_jobs_wait_then_continue_or_stop() {
        let (_t, base, root, src) = setup();
        let id = create_in(&base, &spec("send", &root, vec![src.display().to_string()], "/Projects", "move")).unwrap();
        pause_in(&base, &id).unwrap();
        assert_eq!(read_status(&base.join(&id)).unwrap().state, "paused");
        // Started while paused, it stays paused and moves nothing.
        run_in(&base, &id).unwrap();
        assert_eq!(read_status(&base.join(&id)).unwrap().state, "paused");
        assert!(src.join("reads_0.fastq").exists());
        // Resumed, it runs to the end.
        assert!(resume_in(&base, &id).unwrap(), "this server starts it again");
        assert_eq!(read_status(&base.join(&id)).unwrap().state, "queued");
        run_in(&base, &id).unwrap();
        assert_eq!(read_status(&base.join(&id)).unwrap().state, "done");
        assert!(!src.exists());

        // Stopping a paused job ends it at once.
        let (_t2, base2, root2, src2) = setup();
        let id2 = create_in(&base2, &spec("send", &root2, vec![src2.display().to_string()], "/", "copy")).unwrap();
        pause_in(&base2, &id2).unwrap();
        cancel_in(&base2, &id2).unwrap();
        assert_eq!(read_status(&base2.join(&id2)).unwrap().state, "cancelled");
        assert!(!resume_in(&base2, &id2).unwrap(), "a stopped job doesn't start again");
    }
}
