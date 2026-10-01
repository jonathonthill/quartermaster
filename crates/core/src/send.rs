//! The sender: walks local files and archives them.
//!
//! Order of work:
//! 1. Scan the sources (no reads yet), and create the folder tree in one batch.
//! 2. Resuming: files already at their destination with the same size are
//!    checksummed in the background, while files not there yet are sent first.
//!    Any whose checksum differs are sent after.
//! 3. Files whose size matches stored content are hashed; if the content is
//!    already stored, only a reference is added (deduplication).
//! 4. Everything else is sent: small files grouped into solid blocks, large
//!    files one at a time, each compressed and hashed in a single read.
//! 5. In Move mode, nothing is deleted until the whole transfer is archived
//!    and verified. Then each source file is deleted only if it is provably
//!    unchanged since it was read (same device, inode, size, mtime, and ctime).
//!    If anything failed or the transfer was stopped, every original is kept.
//!
//! Each folder sent becomes a project, unless it's sent into a project (then
//! its files are added to that project). Files can only be sent into a
//! project, or wrapped in a new one with [`SendOptions::new_project`].
//! Deduplication happens within each project.

use std::collections::HashMap;
use std::fs::{self, File, Metadata};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::api::{Archive, Conflict, DirSpec, FileMeta, PutFile, PutSolid, PutStatus, SolidMember};
use crate::catalog::{Entry, NodeKind};
use crate::error::{Error, Result};
use crate::hash::{Digest, hash_reader};
use crate::seekable::{CompressOptions, CompressingReader, looks_incompressible};
use crate::tar::TarWriter;
use crate::util;
use crate::vpath::VPath;

/// OS metadata files. Skipped by default, and in Move mode deleted when they
/// are the only thing left in an otherwise-emptied folder.
pub const SYSTEM_JUNK: &[&str] = &[
    ".DS_Store",
    "._*",
    "Thumbs.db",
    "ehthumbs.db",
    "desktop.ini",
    ".Spotlight-V100",
    ".Trashes",
    ".fseventsd",
    ".TemporaryItems",
    "$RECYCLE.BIN",
    "System Volume Information",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Copy,
    Move,
}

#[derive(Clone, Debug)]
pub struct SendOptions {
    pub mode: Mode,
    pub policy: Conflict,
    /// Job key; reuse it to resume an interrupted transfer into the same pack.
    pub job: String,
    pub compress: CompressOptions,
    /// Files at or below this size are grouped into solid blocks.
    pub small_file_max: u64,
    /// Target uncompressed size of a solid block.
    pub solid_block_max: u64,
    /// Glob patterns (`*`, `?`) matched against file and folder names.
    pub exclude: Vec<String>,
    /// Retries for a file whose verification fails.
    pub retries: u32,
    /// Set to stop the transfer. The file in flight is abandoned (nothing
    /// partial is kept) and what was already archived stays archived.
    pub cancel: Option<Arc<AtomicBool>>,
    /// Put everything sent into a new project folder of this name inside the
    /// destination (for sending loose files outside any project).
    pub new_project: Option<String>,
}

impl Default for SendOptions {
    fn default() -> Self {
        SendOptions {
            mode: Mode::Copy,
            policy: Conflict::Skip,
            job: util::random_hex(8),
            compress: CompressOptions::default(),
            small_file_max: 1 << 20,
            solid_block_max: 64 << 20,
            exclude: SYSTEM_JUNK.iter().map(|s| s.to_string()).collect(),
            retries: 2,
            cancel: None,
            new_project: None,
        }
    }
}

/// A reader that reports how much has been read: every 4 MB, and at the end.
pub struct Counting<'a, R> {
    inner: R,
    read: u64,
    reported: u64,
    report: &'a mut dyn FnMut(u64),
}

impl<'a, R> Counting<'a, R> {
    pub fn new(inner: R, report: &'a mut dyn FnMut(u64)) -> Self {
        Counting { inner, read: 0, reported: 0, report }
    }
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        if (n == 0 && self.read > self.reported) || self.read - self.reported >= 4 << 20 {
            self.reported = self.read;
            (self.report)(self.read);
        }
        Ok(n)
    }
}

/// A reader that fails once `flag` is set, so a long read stops promptly.
pub struct CancelRead<R> {
    pub inner: R,
    pub flag: Option<Arc<AtomicBool>>,
}

impl<R: Read> Read for CancelRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.flag.as_ref().is_some_and(|f| f.load(Ordering::Relaxed)) {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
        }
        self.inner.read(buf)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Scanned {
        files: u64,
        bytes: u64,
        folders: u64,
    },
    Checking {
        path: PathBuf,
    },
    Sending {
        path: PathBuf,
    },
    /// A large file has been sent; the archive is reading it back to check it.
    Verifying {
        path: PathBuf,
    },
    /// Overall progress in original (uncompressed) bytes handled so far.
    Progress {
        done: u64,
        total: u64,
    },
    Archived {
        path: PathBuf,
        dest: VPath,
        status: PutStatus,
    },
    Deleted {
        path: PathBuf,
    },
    Kept {
        path: PathBuf,
        reason: String,
    },
    Failed {
        path: PathBuf,
        error: String,
    },
    Sealing,
    /// A project folder this transfer sends into, and whether this attempt
    /// created it (it didn't exist before).
    Project {
        path: VPath,
        new: bool,
    },
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SendReport {
    pub job: String,
    pub files: u64,
    pub bytes: u64,
    pub stored: u64,
    pub deduplicated: u64,
    pub identical: u64,
    pub skipped: Vec<(PathBuf, VPath)>,
    pub failed: Vec<(PathBuf, String)>,
    pub deleted: u64,
    pub kept: Vec<(PathBuf, String)>,
    /// Compressed bytes sent to the archive.
    pub bytes_sent: u64,
    /// The transfer was stopped before every file was handled.
    pub cancelled: bool,
    /// Move mode: the originals were all kept, because not everything was archived.
    pub originals_kept: bool,
}

impl SendReport {
    pub fn ok(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Identity of a local file, used to prove it did not change after it was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ident {
    size: u64,
    mtime_ns: i64,
    ctime_ns: i64,
    dev: u64,
    ino: u64,
}

fn ident(md: &Metadata) -> Ident {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ident {
            size: md.len(),
            mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(),
            ctime_ns: md.ctime() * 1_000_000_000 + md.ctime_nsec(),
            dev: md.dev(),
            ino: md.ino(),
        }
    }
    #[cfg(not(unix))]
    {
        Ident { size: md.len(), mtime_ns: mtime_ns(md), ctime_ns: ctime_fallback(md), dev: 0, ino: 0 }
    }
}

#[cfg(not(unix))]
fn ctime_fallback(md: &Metadata) -> i64 {
    md.created().ok().map(system_time_ns).unwrap_or(0)
}

fn system_time_ns(t: std::time::SystemTime) -> i64 {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(e) => -(e.duration().as_nanos() as i64),
    }
}

pub fn mtime_ns(md: &Metadata) -> i64 {
    md.modified().map(system_time_ns).unwrap_or(0)
}

pub fn mode_of(md: &Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        if md.is_dir() {
            0o755
        } else if md.permissions().readonly() {
            0o444
        } else {
            0o644
        }
    }
}

/// Match a name against a glob with `*` and `?`.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut pi, mut ni, mut star, mut mark) = (0, 0, None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn is_excluded(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| glob_match(p, name))
}

fn is_junk(name: &str) -> bool {
    SYSTEM_JUNK.iter().any(|p| glob_match(p, name))
}

#[derive(Debug)]
enum ItemKind {
    File,
    Symlink(String),
}

#[derive(Debug)]
struct Item {
    local: PathBuf,
    dest: VPath,
    /// Where the source it came from lands: a path in its project, for deduplication.
    scope: VPath,
    kind: ItemKind,
    meta: FileMeta,
    ident: Ident,
}

struct Plan {
    dirs: Vec<DirSpec>,
    /// Local folders created by this transfer, deepest last.
    local_dirs: Vec<PathBuf>,
    items: Vec<Item>,
    bytes: u64,
}

fn utf8_name(p: &Path) -> Result<&str> {
    p.file_name().and_then(|n| n.to_str()).ok_or_else(|| Error::InvalidPath(format!("{} has a name that isn't valid UTF-8", p.display())))
}

/// Where the sources go: `dest`, or a new project folder inside it.
fn destination(dest: &VPath, opts: &SendOptions) -> Result<VPath> {
    match &opts.new_project {
        Some(name) => dest.join(name),
        None => Ok(dest.clone()),
    }
}

fn scan(sources: &[PathBuf], dest: &VPath, opts: &SendOptions, failed: &mut Vec<(PathBuf, String)>) -> Result<Plan> {
    let mut plan = Plan { dirs: Vec::new(), local_dirs: Vec::new(), items: Vec::new(), bytes: 0 };
    let dest = destination(dest, opts)?;
    if opts.new_project.is_some() {
        plan.dirs.push(DirSpec { path: dest.clone(), mtime_ns: util::now_ns(), mode: 0o755, project: true });
    }
    for src in sources {
        let md = fs::symlink_metadata(src).map_err(|e| Error::NotFound(format!("{}: {e}", src.display())))?;
        let name = utf8_name(src)?;
        let target = dest.join(name)?;
        let first = plan.dirs.len();
        scan_one(src, &md, target.clone(), &target, opts, &mut plan, failed);
        // The folder sent is a project's top folder (or, sent into a project, part of it).
        if let Some(d) = plan.dirs.get_mut(first).filter(|d| d.path == target) {
            d.project = true;
        }
    }
    Ok(plan)
}

fn scan_one(
    local: &Path,
    md: &Metadata,
    dest: VPath,
    scope: &VPath,
    opts: &SendOptions,
    plan: &mut Plan,
    failed: &mut Vec<(PathBuf, String)>,
) {
    let ft = md.file_type();
    if ft.is_dir() {
        plan.dirs.push(DirSpec { path: dest.clone(), mtime_ns: mtime_ns(md), mode: mode_of(md), project: false });
        plan.local_dirs.push(local.to_path_buf());
        let mut children: Vec<PathBuf> = match fs::read_dir(local) {
            Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
            Err(e) => {
                failed.push((local.to_path_buf(), format!("can't read folder: {e}")));
                return;
            }
        };
        children.sort();
        for child in children {
            let name = match utf8_name(&child) {
                Ok(n) => n.to_string(),
                Err(e) => {
                    failed.push((child, e.to_string()));
                    continue;
                }
            };
            if is_excluded(&name, &opts.exclude) {
                continue;
            }
            let cmd = match fs::symlink_metadata(&child) {
                Ok(m) => m,
                Err(e) => {
                    failed.push((child, e.to_string()));
                    continue;
                }
            };
            match dest.join(&name) {
                Ok(d) => scan_one(&child, &cmd, d, scope, opts, plan, failed),
                Err(e) => failed.push((child, e.to_string())),
            }
        }
    } else if ft.is_file() {
        plan.bytes += md.len();
        plan.items.push(Item {
            local: local.to_path_buf(),
            dest,
            scope: scope.clone(),
            kind: ItemKind::File,
            meta: FileMeta { size: md.len(), mtime_ns: mtime_ns(md), mode: mode_of(md) },
            ident: ident(md),
        });
    } else if ft.is_symlink() {
        match fs::read_link(local).map(|t| t.to_str().map(str::to_string)) {
            Ok(Some(t)) => plan.items.push(Item {
                local: local.to_path_buf(),
                dest,
                scope: scope.clone(),
                kind: ItemKind::Symlink(t),
                meta: FileMeta { size: 0, mtime_ns: mtime_ns(md), mode: 0o777 },
                ident: ident(md),
            }),
            Ok(None) => failed.push((local.to_path_buf(), "link target isn't valid UTF-8".into())),
            Err(e) => failed.push((local.to_path_buf(), e.to_string())),
        }
    } else {
        failed.push((local.to_path_buf(), "not a regular file, folder, or link (skipped)".into()));
    }
}

/// Hash a local file, returning the digest only if the file didn't change while reading.
fn hash_stable(path: &Path, before: &Ident) -> Result<Option<Digest>> {
    let f = File::open(path)?;
    let (d, _) = hash_reader(f)?;
    let after = ident(&fs::symlink_metadata(path)?);
    Ok((after == *before).then_some(d))
}

struct Sender<'a> {
    archive: &'a mut dyn Archive,
    opts: &'a SendOptions,
    on: &'a mut dyn FnMut(&Event),
    report: SendReport,
    done: u64,
    total: u64,
    /// The connection to the archive dropped: stop, and report that instead.
    lost: Option<String>,
    /// Move mode: archived files to delete once the whole transfer is done.
    to_delete: Vec<(PathBuf, Ident)>,
    /// Time spent sending (reading, compressing, and the network), for the timing log.
    sending: Duration,
    /// Time spent waiting for the archive to finish checking what it received.
    settling: Duration,
}

impl Sender<'_> {
    fn cancelled(&mut self) -> bool {
        if self.lost.is_some() {
            return true;
        }
        let c = self.opts.cancel.as_ref().is_some_and(|f| f.load(Ordering::Relaxed));
        if c {
            self.report.cancelled = true;
        }
        c
    }

    fn emit(&mut self, e: Event) {
        (self.on)(&e);
    }

    fn progress(&mut self, bytes: u64) {
        self.done += bytes;
        let (done, total) = (self.done, self.total);
        self.emit(Event::Progress { done, total });
    }

    fn fail(&mut self, path: &Path, err: impl ToString) {
        let error = err.to_string();
        if self.lost.is_none() && error.starts_with("lost the connection") {
            self.lost = Some(error.clone());
        }
        self.emit(Event::Failed { path: path.to_path_buf(), error: error.clone() });
        self.report.failed.push((path.to_path_buf(), error));
    }

    /// Record an archive acknowledgement. In Move mode, the source is deleted
    /// once the whole transfer is done (see `delete_originals`).
    fn acked(&mut self, item: &Item, dest: VPath, status: PutStatus) {
        match status {
            PutStatus::Stored => self.report.stored += 1,
            PutStatus::Deduplicated => self.report.deduplicated += 1,
            PutStatus::Identical => self.report.identical += 1,
            PutStatus::Skipped => self.report.skipped.push((item.local.clone(), dest.clone())),
        }
        self.emit(Event::Archived { path: item.local.clone(), dest, status });
        if self.opts.mode != Mode::Move {
            return;
        }
        if !status.is_archived() {
            self.kept(&item.local, "a different item already exists at the destination".into());
            return;
        }
        self.to_delete.push((item.local.clone(), item.ident));
    }

    /// Move mode, after everything is archived: delete each source that is
    /// provably the same file that was read.
    fn delete_originals(&mut self) {
        for (path, id) in std::mem::take(&mut self.to_delete) {
            match fs::symlink_metadata(&path) {
                Ok(md) if ident(&md) == id => match fs::remove_file(&path) {
                    Ok(()) => {
                        self.report.deleted += 1;
                        self.emit(Event::Deleted { path });
                    }
                    Err(e) => self.kept(&path, format!("couldn't delete: {e}")),
                },
                Ok(_) => self.kept(&path, "changed after it was read; kept so nothing is lost".into()),
                Err(e) => self.kept(&path, format!("couldn't re-check before deleting: {e}")),
            }
        }
    }

    /// Take back an acknowledgement: the archive's check of this file failed, so it wasn't kept.
    fn unack(&mut self, item: &Item) {
        self.report.stored = self.report.stored.saturating_sub(1);
        self.to_delete.retain(|(p, _)| p != &item.local);
    }

    fn kept(&mut self, path: &Path, reason: String) {
        self.emit(Event::Kept { path: path.to_path_buf(), reason: reason.clone() });
        self.report.kept.push((path.to_path_buf(), reason));
    }

    /// Reference already-stored content for a batch of files. With `resend`,
    /// a file whose content turns out to be missing (its first copy failed to
    /// send) is sent in its own right instead.
    fn link_batch(&mut self, items: &[(&Item, Digest)], resend: bool) {
        let reqs: Vec<(PutFile, Digest)> = items.iter().map(|(i, d)| (self.put_request(i), *d)).collect();
        let results = match self.archive.link_many(&self.opts.job, &reqs) {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                for (item, _) in items {
                    self.progress(item.meta.size);
                    self.fail(&item.local, &msg);
                }
                return;
            }
        };
        for ((item, _), res) in items.iter().zip(results) {
            match res {
                Ok(out) => {
                    self.progress(item.meta.size);
                    self.acked(item, out.dest, out.status);
                }
                Err(Error::NotFound(_)) if resend => self.send_large(item),
                Err(e) => {
                    self.progress(item.meta.size);
                    self.fail(&item.local, e);
                }
            }
        }
    }

    fn put_request(&self, item: &Item) -> PutFile {
        PutFile { dest: item.dest.clone(), meta: item.meta, policy: self.opts.policy }
    }

    fn send_large(&mut self, item: &Item) {
        self.emit(Event::Sending { path: item.local.clone() });
        let mut attempt = 0;
        let mut policy = self.opts.policy;
        loop {
            match self.try_send_large(item, policy) {
                Ok(Some((out, sent))) => {
                    self.report.bytes_sent += sent;
                    self.progress(item.meta.size);
                    self.acked(item, out.dest, out.status);
                    return;
                }
                // The file changed while it was read, so the stored copy may be
                // inconsistent. Send it again, replacing that copy (it goes to the trash).
                Ok(None) if attempt < self.opts.retries => {
                    attempt += 1;
                    policy = Conflict::Replace;
                }
                Ok(None) => {
                    self.progress(item.meta.size);
                    self.fail(&item.local, "kept changing while it was being read; the archived copy may not match it");
                    return;
                }
                Err(Error::Verify(_)) if attempt < self.opts.retries => attempt += 1,
                Err(e) => {
                    self.progress(item.meta.size);
                    self.fail(&item.local, e);
                    return;
                }
            }
        }
    }

    /// Returns `None` if the file changed while it was read.
    fn try_send_large(&mut self, item: &Item, policy: Conflict) -> Result<Option<(crate::api::PutOutcome, u64)>> {
        let mut f = File::open(&item.local)?;
        let before = ident(&f.metadata()?);
        let mut sample = vec![0u8; before.size.min(4 << 20) as usize];
        f.read_exact(&mut sample)?;
        f.seek(SeekFrom::Start(0))?;
        let mut copts = self.opts.compress;
        if looks_incompressible(&sample) {
            copts.level = copts.level.min(1);
        }
        // Progress within the file, so a large one doesn't look stalled.
        let (base, total, size) = (self.done, self.total, before.size);
        let on = &mut *self.on;
        let mut report = |n: u64| {
            on(&Event::Progress { done: base + n, total });
            if n >= size && size >= 64 << 20 {
                on(&Event::Verifying { path: item.local.clone() });
            }
        };
        let counted = Counting::new(CancelRead { inner: f, flag: self.opts.cancel.clone() }, &mut report);
        let mut payload = CompressingReader::new(counted, copts)?;
        let meta = FileMeta { size: before.size, mtime_ns: before.mtime_ns, ..item.meta };
        let req = PutFile { dest: item.dest.clone(), meta, policy };
        let t = Instant::now();
        let out = self.archive.put_file(&self.opts.job, &req, &mut payload);
        self.sending += t.elapsed();
        let out = out?;
        let sent = payload.compressed_len();
        drop(payload);
        let after = ident(&fs::symlink_metadata(&item.local)?);
        if after != before && out.status == PutStatus::Stored {
            return Ok(None);
        }
        Ok(Some((out, sent)))
    }

    fn send_solid(&mut self, batch: &mut Vec<(&Item, Vec<u8>, Digest)>) {
        if batch.is_empty() {
            return;
        }
        let items: Vec<(&Item, Vec<u8>, Digest)> = std::mem::take(batch);
        let mut tar = TarWriter::new(Cursor::new(Vec::new()), 0);
        let mut members = Vec::with_capacity(items.len());
        for (item, data, sha) in &items {
            let secs = item.meta.mtime_ns.div_euclid(1_000_000_000);
            if let Err(e) = tar.append(&item.dest.to_rel_string(), item.meta.mode, secs, data) {
                self.fail(&item.local, e);
                return;
            }
            members.push(SolidMember { dest: item.dest.clone(), meta: item.meta, sha256: *sha });
        }
        if let Err(e) = tar.finish() {
            self.fail(&items[0].0.local, e);
            return;
        }
        let bytes = tar.into_inner().into_inner();
        let req = PutSolid { members, policy: self.opts.policy };
        let mut attempt = 0;
        let result = loop {
            let mut payload = match CompressingReader::new(Cursor::new(&bytes), self.opts.compress) {
                Ok(p) => p,
                Err(e) => break Err(Error::from(e)),
            };
            let t = Instant::now();
            let put = self.archive.put_solid(&self.opts.job, &req, &mut payload);
            self.sending += t.elapsed();
            match put {
                Err(Error::Verify(_)) if attempt < self.opts.retries => attempt += 1,
                other => {
                    if other.is_ok() {
                        self.report.bytes_sent += payload.compressed_len();
                    }
                    break other;
                }
            }
        };
        // (Progress was counted as each file was read into the block.)
        match result {
            Ok(outs) => {
                for ((item, _, _), out) in items.iter().zip(outs) {
                    self.acked(item, out.dest, out.status);
                }
            }
            Err(e) => {
                let msg = e.to_string();
                for (item, _, _) in &items {
                    self.fail(&item.local, &msg);
                }
            }
        }
    }
}

/// Archive `sources` (files or folders) into the folder `dest`.
pub fn send(
    archive: &mut dyn Archive,
    sources: &[PathBuf],
    dest: &VPath,
    opts: &SendOptions,
    on: &mut dyn FnMut(&Event),
) -> Result<SendReport> {
    let mut failed = Vec::new();
    let plan = scan(sources, dest, opts, &mut failed)?;
    let n_files = plan.items.iter().filter(|i| matches!(i.kind, ItemKind::File)).count() as u64;
    on(&Event::Scanned { files: n_files, bytes: plan.bytes, folders: plan.dirs.len() as u64 });

    let mut s = Sender {
        archive,
        opts,
        on,
        report: SendReport { job: opts.job.clone(), files: n_files, bytes: plan.bytes, ..Default::default() },
        done: 0,
        total: plan.bytes,
        lost: None,
        to_delete: Vec::new(),
        sending: Duration::ZERO,
        settling: Duration::ZERO,
    };
    let started = Instant::now();
    for (p, e) in failed {
        s.fail(&p, e);
    }

    // Note which project folders this attempt creates (so an abandoned
    // transfer can remove exactly those), then create the folder tree.
    for d in plan.dirs.iter().filter(|d| d.project) {
        let new = s.archive.stat(&d.path)?.is_none();
        s.emit(Event::Project { path: d.path.clone(), new });
    }
    s.archive.mkdirs(&opts.job, &plan.dirs)?;

    // What's already at the destination (for resume and conflicts), one listing per source.
    let mut existing: HashMap<VPath, Entry> = HashMap::new();
    let dest = &destination(dest, opts)?;
    for src in sources {
        let Ok(name) = utf8_name(src) else { continue };
        let top = dest.join(name)?;
        match s.archive.stat(&top)? {
            Some(e) if e.kind == NodeKind::Dir => {
                for (rel, e) in s.archive.walk(&top)? {
                    existing.insert(top.join_rel(&rel)?, e);
                }
            }
            Some(e) => {
                existing.insert(top, e);
            }
            None => {}
        }
    }

    // Resume: files already at the destination with the same size are
    // checksummed in the background; files not there yet go first.
    let mut pending: Vec<&Item> = Vec::new();
    let mut to_check: Vec<&Item> = Vec::new();
    for item in &plan.items {
        match (&item.kind, existing.get(&item.dest)) {
            (ItemKind::File, Some(e)) if e.kind == NodeKind::File && e.size == item.meta.size => to_check.push(item),
            _ => pending.push(item),
        }
    }
    let mut checks = Checks::start(&to_check, opts.cancel.clone());
    let mut resend: Vec<&Item> = Vec::new();

    // Deduplicate within each project. Hash only files that could be
    // duplicates: their size is already in the project, or another file in
    // this transfer has the same size.
    let files: Vec<&Item> = pending.iter().copied().filter(|i| matches!(i.kind, ItemKind::File)).collect();
    let sizes: Vec<u64> = files.iter().map(|i| i.meta.size).collect();
    let mut present = vec![false; files.len()];
    for (scope, idx) in by_scope(&files) {
        for chunk in idx.chunks(10_000) {
            let sz: Vec<u64> = chunk.iter().map(|&i| sizes[i]).collect();
            for (&i, p) in chunk.iter().zip(s.archive.sizes_present(scope, &sz)?) {
                present[i] = p;
            }
        }
    }
    let mut size_count: HashMap<u64, u32> = HashMap::new();
    for &sz in &sizes {
        *size_count.entry(sz).or_default() += 1;
    }
    let mut digest_of: HashMap<*const Item, Digest> = HashMap::new();
    for (item, in_archive) in files.iter().zip(&present) {
        if *in_archive || size_count[&item.meta.size] > 1 {
            s.emit(Event::Checking { path: item.local.clone() });
            if let Ok(Some(d)) = hash_stable(&item.local, &item.ident) {
                digest_of.insert(*item as *const Item, d);
            }
        }
    }
    let mut stored_already: std::collections::HashSet<(&VPath, Digest)> = std::collections::HashSet::new();
    let hashed: Vec<&Item> = files.iter().copied().filter(|i| digest_of.contains_key(&(*i as *const Item))).collect();
    for (scope, idx) in by_scope(&hashed) {
        let mut candidates: Vec<Digest> = idx.iter().map(|&i| digest_of[&(hashed[i] as *const Item)]).collect();
        candidates.sort_unstable_by_key(|d| d.0);
        candidates.dedup();
        for chunk in candidates.chunks(1000) {
            for (d, h) in chunk.iter().zip(s.archive.have(scope, chunk)?) {
                if h {
                    stored_already.insert((scope, *d));
                }
            }
        }
    }

    // Content already in the project: add references in batches.
    let mut already: Vec<(&Item, Digest)> = Vec::new();
    pending.retain(|item| match digest_of.get(&(*item as *const Item)) {
        Some(d) if stored_already.contains(&(&item.scope, *d)) => {
            already.push((item, *d));
            false
        }
        _ => true,
    });
    for chunk in already.chunks(1000) {
        if s.cancelled() {
            break;
        }
        s.link_batch(chunk, false);
    }

    // Send. A file whose content was already sent earlier in this transfer is
    // linked after the first copy is committed (or, if that copy went to
    // another project, sent after all). Small files are grouped per source,
    // so a block never spans projects.
    let mut batch: Vec<(&Item, Vec<u8>, Digest)> = Vec::new();
    let mut batch_bytes = 0u64;
    let mut links: Vec<&Item> = Vec::new();
    let mut seen: std::collections::HashSet<Digest> = std::collections::HashSet::new();
    let mut deferred: Vec<(&Item, Digest)> = Vec::new();
    for item in pending {
        if s.cancelled() {
            break;
        }
        checks.absorb(&mut s, &to_check, &existing, &mut resend, false);
        if let ItemKind::Symlink(_) = item.kind {
            links.push(item);
            continue;
        }
        if let Some(d) = digest_of.get(&(item as *const Item)).copied() {
            if !seen.insert(d) {
                deferred.push((item, d));
                continue;
            }
        }
        if item.meta.size <= opts.small_file_max {
            if batch.first().is_some_and(|(b, _, _)| b.scope != item.scope) {
                s.send_solid(&mut batch);
                batch_bytes = 0;
            }
            s.emit(Event::Sending { path: item.local.clone() });
            match read_small(item) {
                Ok(Some(data)) => {
                    let d = Digest::of(&data);
                    // Count it now, so progress moves file by file rather than block by block.
                    s.progress(item.meta.size);
                    batch_bytes += data.len() as u64;
                    batch.push((item, data, d));
                    if batch_bytes >= opts.solid_block_max {
                        s.send_solid(&mut batch);
                        batch_bytes = 0;
                    }
                }
                Ok(None) => {
                    s.progress(item.meta.size);
                    s.kept(&item.local, "changed while it was being read; it will be sent next time".into());
                }
                Err(e) => {
                    s.progress(item.meta.size);
                    s.fail(&item.local, e);
                }
            }
        } else {
            s.send_large(item);
        }
    }
    s.send_solid(&mut batch);

    // The rest of the background checks. Files whose content differs from what's
    // stored (or that changed) are sent now, like any other.
    if !s.cancelled() {
        checks.absorb(&mut s, &to_check, &existing, &mut resend, true);
    }
    for item in resend {
        if s.cancelled() {
            break;
        }
        s.send_large(item);
    }

    // The archive checks what it receives while more arrives. Any file whose
    // check failed wasn't kept: send it once more, then report it if it fails again.
    for round in 0..2 {
        if s.cancelled() {
            break;
        }
        let t = Instant::now();
        let failed = s.archive.settle()?;
        s.settling += t.elapsed();
        for (dest, why) in failed {
            let Some(item) = plan.items.iter().find(|i| i.dest == dest) else { continue };
            s.unack(item);
            if round == 0 {
                s.send_large(item);
            } else {
                s.fail(&item.local, format!("failed its check on the archive twice: {why}"));
            }
        }
    }

    for chunk in deferred.chunks(1000) {
        if s.cancelled() {
            break;
        }
        s.link_batch(chunk, true);
    }

    for item in links {
        if s.cancelled() {
            break;
        }
        if let ItemKind::Symlink(target) = &item.kind {
            match s.archive.symlink(&opts.job, &item.dest, target, item.meta.mtime_ns, opts.policy) {
                Ok(out) => s.acked(item, out.dest, out.status),
                Err(e) => s.fail(&item.local, e),
            }
        }
    }

    if let Some(e) = s.lost.take() {
        // Files not yet sent stay put; sending again resumes where this stopped.
        return Err(Error::Other(e));
    }
    s.emit(Event::Sealing);
    let sealing = Instant::now();
    s.archive.finish_job(&opts.job)?;
    util::log_timing(&format!(
        "send job={} files={} bytes={} sent_bytes={} total_s={:.1} sending_s={:.1} waiting_for_checks_s={:.1} sealing_s={:.1} resume_checks={}",
        opts.job,
        s.report.files,
        s.report.bytes,
        s.report.bytes_sent,
        started.elapsed().as_secs_f64(),
        s.sending.as_secs_f64(),
        s.settling.as_secs_f64(),
        sealing.elapsed().as_secs_f64(),
        to_check.len(),
    ));

    // Move: only a complete transfer deletes the originals.
    if opts.mode == Mode::Move {
        if s.report.failed.is_empty() && !s.report.cancelled && checks.left == 0 {
            s.delete_originals();
            remove_emptied_dirs(&plan.local_dirs);
        } else if !s.to_delete.is_empty() {
            s.report.originals_kept = true;
        }
    }
    Ok(s.report)
}

/// Checksums of files that may already be archived (resuming), read on a
/// background thread while other files are sent.
struct Checks {
    rx: mpsc::Receiver<(usize, Result<Option<Digest>>)>,
    /// Results not yet handled.
    left: usize,
}

impl Checks {
    fn start(items: &[&Item], cancel: Option<Arc<AtomicBool>>) -> Checks {
        let (tx, rx) = mpsc::channel();
        let work: Vec<(usize, PathBuf, Ident)> = items.iter().enumerate().map(|(i, it)| (i, it.local.clone(), it.ident)).collect();
        let left = work.len();
        if left > 0 {
            std::thread::spawn(move || {
                for (i, path, id) in work {
                    if cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
                        break;
                    }
                    if tx.send((i, hash_stable(&path, &id))).is_err() {
                        break;
                    }
                }
            });
        }
        Checks { rx, left }
    }

    /// Handle finished checks: a match is done; anything else goes to `resend`.
    /// With `block`, wait for all of them (or until the checks stop).
    fn absorb<'a>(
        &mut self,
        s: &mut Sender,
        items: &[&'a Item],
        existing: &HashMap<VPath, Entry>,
        resend: &mut Vec<&'a Item>,
        block: bool,
    ) {
        while self.left > 0 {
            let got = if block { self.rx.recv().ok() } else { self.rx.try_recv().ok() };
            let Some((i, res)) = got else { break };
            self.left -= 1;
            let item = items[i];
            match res {
                Ok(Some(d)) if Some(d) == existing.get(&item.dest).and_then(|e| e.sha256) => {
                    s.progress(item.meta.size);
                    s.acked(item, item.dest.clone(), PutStatus::Identical);
                }
                Ok(_) => resend.push(item),
                Err(e) => {
                    s.progress(item.meta.size);
                    s.fail(&item.local, e);
                }
            }
        }
    }
}

/// Items grouped by scope (in first-seen order), as indexes into `items`.
fn by_scope<'a>(items: &[&'a Item]) -> Vec<(&'a VPath, Vec<usize>)> {
    let mut groups: Vec<(&VPath, Vec<usize>)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        match groups.iter_mut().find(|(s, _)| *s == &item.scope) {
            Some((_, v)) => v.push(i),
            None => groups.push((&item.scope, vec![i])),
        }
    }
    groups
}

/// Read a small file whole, returning `None` if it changed while reading.
fn read_small(item: &Item) -> Result<Option<Vec<u8>>> {
    let mut f = File::open(&item.local)?;
    if ident(&f.metadata()?) != item.ident {
        return Ok(None);
    }
    let mut data = Vec::with_capacity(item.meta.size as usize);
    f.read_to_end(&mut data)?;
    let after = ident(&fs::symlink_metadata(&item.local)?);
    Ok((after == item.ident && data.len() as u64 == item.meta.size).then_some(data))
}

/// After a Move, remove source folders that are now empty apart from OS junk
/// files. Deepest first; any folder with something else left in it is kept.
fn remove_emptied_dirs(dirs: &[PathBuf]) {
    for dir in dirs.iter().rev() {
        let Ok(rd) = fs::read_dir(dir) else { continue };
        let entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        let only_junk =
            entries.iter().all(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false) && e.file_name().to_str().is_some_and(is_junk));
        if !only_junk {
            continue;
        }
        for e in &entries {
            let _ = fs::remove_file(e.path());
        }
        let _ = fs::remove_dir(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("._*", "._foo"));
        assert!(!glob_match("._*", "_foo"));
        assert!(glob_match("*.tmp", "a.b.tmp"));
        assert!(glob_match("?.txt", "a.txt"));
        assert!(!glob_match("?.txt", "ab.txt"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b*c", "aXXbYY"));
    }
}
