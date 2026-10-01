//! An archive on disk: `archive.json`, `catalog.db`, and `packs/`.
//!
//! Writes follow one rule: bytes go into the open pack, are fsynced, read back,
//! decompressed, and checked against the sender's SHA-256 *before* the catalog
//! commit that makes them visible. A failed check truncates the pack back to
//! where it was. The committed pack length lives in the catalog, so a crash
//! mid-write is undone by truncating to that length when the job resumes.
//!
//! Archived projects are frozen. A folder sent to the archive becomes a
//! project; after that, nothing inside it can be renamed, moved, replaced, or
//! deleted on its own, though new files can be added. The project as a whole
//! can be renamed, moved between organizing folders, or moved to the trash.
//! Each pack holds one project's data, so deleting a project frees whole
//! packs, and each project can be recovered from its own packs.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::api::{Archive, Conflict, DirSpec, FileMeta, Payload, PutFile, PutOutcome, PutSolid, PutStatus};
use crate::catalog::{self as cat, Catalog, Entry, NewFile, NodeKind, PackEntry, PackState};
use crate::error::{Error, Result};
use crate::hash::{Digest, hash_reader};
use crate::recovery;
use crate::seekable::{FrameCache, RangeReader};
use crate::tar::{self, Kind, TarWriter};
use crate::util::{self, now_secs};
use crate::vpath::VPath;

pub const FORMAT_VERSION: u32 = 1;
const CONFIG_FILE: &str = "archive.json";
const CATALOG_FILE: &str = "catalog.db";
const PACKS_DIR: &str = "packs";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub format: u32,
    pub archive_id: String,
    pub created: i64,
    /// A pack is sealed once it grows past this size.
    pub pack_target_bytes: u64,
    /// PAR2 recovery data as a percentage of each pack.
    pub par2_redundancy_percent: u32,
    /// Days an item stays in the trash before it is purged.
    pub trash_days: u32,
    /// par2 program to use; if unset, common locations and PATH are searched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub par2_path: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            format: FORMAT_VERSION,
            archive_id: util::random_hex(16),
            created: now_secs(),
            pack_target_bytes: 32 << 30,
            par2_redundancy_percent: 10,
            trash_days: 30,
            par2_path: None,
        }
    }
}

/// The stored frames covering one file, from [`Store::frame_plan`].
pub struct FramePlan {
    pub file: File,
    /// Offset of the compressed stream in the pack.
    pub start: u64,
    pub frames: Vec<crate::seekable::Frame>,
    /// Bytes to drop from the start of the first decompressed frame.
    pub skip: u64,
    pub len: u64,
}

struct OpenPack {
    job: String,
    project: String,
    id: i64,
    path: PathBuf,
    tar: TarWriter<BufWriter<File>>,
    solid_seq: u32,
}

pub struct Store {
    root: PathBuf,
    cfg: Config,
    cat: Catalog,
    open: Option<OpenPack>,
    cache: FrameCache,
    /// Written, not yet checked and committed (oldest first). See [`Pending`].
    pending: VecDeque<Pending>,
    /// Files whose read-back check failed since the last `settle` (destination, reason).
    failed_checks: Vec<(VPath, String)>,
    timing: Timing,
}

/// A file (or block of small files) written to the open pack whose read-back
/// check is running on its own thread, so the next one can arrive meanwhile.
/// It's committed to the catalog, in order, once its check passes. Until then
/// its bytes lie past the pack's committed length, so a crash drops them and
/// the sender sends them again. A failed check discards it and everything
/// written after it.
struct Pending {
    start: u64,
    check: JoinHandle<(Result<Checked>, Duration)>,
    put: PendingPut,
}

enum Checked {
    File { digest: Digest, raw_len: u64 },
    Solid { raw_len: u64, offsets: Vec<u64> },
}

enum PendingPut {
    File { job: String, project: String, req: PutFile, member: String, data_off: u64, len: u64, sent: Digest, pack_id: i64, end: u64 },
    Solid { job: String, project: String, req: PutSolid, pre: Vec<Option<PutOutcome>>, member: String, data_off: u64, len: u64, pack_id: i64, end: u64 },
}

impl PendingPut {
    fn dests(&self) -> Vec<VPath> {
        match self {
            PendingPut::File { req, .. } => vec![req.dest.clone()],
            PendingPut::Solid { req, pre, .. } => req.members.iter().zip(pre).filter(|(_, p)| p.is_none()).map(|(m, _)| m.dest.clone()).collect(),
        }
    }
}

/// Read-back checks running at once, each on its own core.
const MAX_PENDING: usize = 4;

/// Where a transfer's time went on this side, logged when its job finishes.
#[derive(Default)]
struct Timing {
    files: u64,
    bytes: u64,
    /// Receiving and writing (including fsync).
    receive: Duration,
    /// Read-back checks, summed over their threads.
    check: Duration,
    /// Time spent waiting for checks to finish.
    wait: Duration,
    commit: Duration,
}

#[derive(Serialize)]
struct PackHeader<'a> {
    format: u32,
    archive_id: &'a str,
    pack: &'a str,
    created: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<ProjectRef>,
    note: &'a str,
}

/// Which project a pack belongs to, and where the project was in the archive
/// when the pack was written. Entry paths in the pack start with `path`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRef {
    pub id: String,
    pub path: String,
}

#[derive(Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub archive_id: String,
    pub pack: String,
    pub sealed_at: i64,
    /// Absent in packs written before projects existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectRef>,
    pub entries: Vec<PackEntry>,
}

/// SQLite's write-ahead log is only safe on a disk the computer running it
/// manages; over a network share, two computers can corrupt the catalog.
fn refuse_network_share(root: &Path) -> Result<()> {
    let existing = root.ancestors().find(|p| p.exists()).unwrap_or(root);
    match util::network_fs(existing) {
        Some(fs) => Err(Error::other(format!(
            "{} is on a network share ({fs}), where the archive's catalog could be damaged. \
             Use ssh://server/path instead, so the server opens it on its own disk",
            root.display()
        ))),
        None => Ok(()),
    }
}

/// Refusal for changes inside an archived project.
fn frozen(path: &VPath) -> Error {
    Error::InvalidPath(format!("{path} is inside an archived project, and archived projects can't be changed"))
}

fn outside_projects(path: &VPath) -> Error {
    Error::InvalidPath(format!(
        "{path} isn't inside a project. Send a folder (it becomes a project), or send files into an existing project"
    ))
}

/// Where a project's top folder is now, relative to the archive root.
fn project_ref(c: &Connection, project: &str) -> Result<Option<ProjectRef>> {
    if project.is_empty() {
        return Ok(None);
    }
    let root = cat::project_root(c, project)?.ok_or_else(|| Error::NotFound(format!("project {project}")))?;
    let (path, _) = cat::path_of(c, root)?;
    Ok(Some(ProjectRef { id: project.to_string(), path: path.to_rel_string() }))
}

/// The project a new item at `dest` goes into; items may only be added inside projects.
fn project_of_dest(c: &Connection, dest: &VPath) -> Result<String> {
    cat::project_for(c, &dest.parent().unwrap_or_default())?.ok_or_else(|| outside_projects(dest))
}

/// True if `existing` is a file stored by `job` in a pack still open, i.e. by
/// this same transfer, which may replace it (a file that changed while it was
/// read is sent again).
fn stored_by_this_job(c: &Connection, existing: &Entry, job: &str) -> Result<bool> {
    let Some(cid) = existing.content_id else { return Ok(false) };
    Ok(c
        .query_row(
            "SELECT 1 FROM contents c JOIN blobs b ON b.id = c.blob_id JOIN packs p ON p.id = b.pack_id \
             WHERE c.id = ?1 AND p.job = ?2 AND p.state = 'open'",
            params![cid, job],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

impl Store {
    pub fn init(root: &Path, cfg: Config) -> Result<Store> {
        refuse_network_share(root)?;
        fs::create_dir_all(root)?;
        if root.join(CONFIG_FILE).exists() {
            return Err(Error::AlreadyExists(format!("an archive already exists at {}", root.display())));
        }
        fs::create_dir_all(root.join(PACKS_DIR))?;
        util::atomic_write(&root.join(CONFIG_FILE), &serde_json::to_vec_pretty(&cfg)?)?;
        let store = Store::open(root)?;
        store.cat.meta_set("archive_id", &cfg.archive_id)?;
        Ok(store)
    }

    pub fn open(root: &Path) -> Result<Store> {
        refuse_network_share(root)?;
        let cfg_bytes = fs::read(root.join(CONFIG_FILE)).map_err(|e| Error::NotFound(format!("no archive at {} ({e})", root.display())))?;
        let cfg: Config = serde_json::from_slice(&cfg_bytes)?;
        if cfg.format > FORMAT_VERSION {
            return Err(Error::other(format!("archive format {} is newer than this program supports", cfg.format)));
        }
        let cat = Catalog::open(&root.join(CATALOG_FILE))?;
        Ok(Store {
            root: root.to_path_buf(),
            cfg,
            cat,
            open: None,
            cache: FrameCache::new(64 << 20),
            pending: VecDeque::new(),
            failed_checks: Vec::new(),
            timing: Timing::default(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn catalog(&self) -> &Catalog {
        &self.cat
    }

    pub fn conn(&self) -> &Connection {
        self.cat.conn()
    }

    pub fn catalog_mut(&mut self) -> &mut Catalog {
        &mut self.cat
    }

    // -----------------------------------------------------------------------
    // Tree edits (catalog only; packs are never rewritten for these). Only
    // organizing folders, whole projects, and data from before projects can
    // be changed; see the module notes.
    // -----------------------------------------------------------------------

    /// Create an organizing folder (and missing parents) outside every project.
    pub fn mkdir_p(&mut self, path: &VPath) -> Result<()> {
        let tx = self.cat.conn_mut().transaction()?;
        if cat::project_for(&tx, path)?.is_some() {
            return Err(frozen(path));
        }
        cat::mkdir_p(&tx, path)?;
        tx.commit()?;
        Ok(())
    }

    /// Rename or move `from` to `to` (the full new path).
    pub fn rename(&mut self, from: &VPath, to: &VPath) -> Result<()> {
        let e = cat::resolve(self.conn(), from)?.ok_or_else(|| Error::NotFound(from.to_string()))?;
        if e.in_project {
            return Err(frozen(from));
        }
        let name = to.name().ok_or_else(|| Error::InvalidPath("can't move to the archive root".into()))?;
        let parent = cat::resolve_dir(self.conn(), &to.parent().unwrap_or_default())?;
        if parent.project.is_some() {
            return Err(Error::InvalidPath(format!(
                "can't move {from} into {}, which is inside an archived project",
                to.parent().unwrap_or_default()
            )));
        }
        self.settle_projects_under(e.id, from)?;
        let tx = self.cat.conn_mut().transaction()?;
        cat::move_node(&tx, e.id, parent.id, name)?;
        tx.commit()?;
        Ok(())
    }

    /// Move an item to the trash. Returns its trash id (for restore).
    pub fn trash(&mut self, path: &VPath) -> Result<i64> {
        let e = cat::resolve(self.conn(), path)?.ok_or_else(|| Error::NotFound(path.to_string()))?;
        if e.id == cat::ROOT_ID {
            return Err(Error::InvalidPath("can't trash the whole archive".into()));
        }
        if e.in_project {
            return Err(frozen(path));
        }
        self.settle_projects_under(e.id, path)?;
        let tx = self.cat.conn_mut().transaction()?;
        cat::trash(&tx, e.id)?;
        tx.commit()?;
        Ok(e.id)
    }

    pub fn restore(&mut self, trash_id: i64, to: Option<&VPath>) -> Result<VPath> {
        let tx = self.cat.conn_mut().transaction()?;
        let e = cat::entry(&tx, trash_id)?;
        let dest = match to {
            Some(p) => p.clone(),
            None => VPath::parse(e.trash_origin.as_deref().unwrap_or(""))?,
        };
        let parent = dest.parent().unwrap_or_default();
        if cat::project_for(&tx, &parent)?.is_some() {
            return Err(Error::InvalidPath(format!("can't restore into {parent}, which is inside an archived project")));
        }
        let p = cat::restore(&tx, trash_id, Some(&dest))?;
        tx.commit()?;
        Ok(p)
    }

    /// Before a project moves or goes to the trash, seal the packs it still
    /// has open (from interrupted transfers), so every pack's paths match
    /// where the project was when the pack was written. Refuses while a
    /// transfer is writing to the project.
    fn settle_projects_under(&mut self, node: i64, path: &VPath) -> Result<()> {
        let projects = cat::projects_under(self.conn(), node)?;
        if projects.is_empty() {
            return Ok(());
        }
        self.release_open()?;
        for p in cat::packs_in_state(self.conn(), PackState::Open)? {
            if !p.project.as_ref().is_some_and(|id| projects.contains(id)) {
                continue;
            }
            if !self.adopt_pack(&p)? {
                return Err(Error::other(format!("{path} is receiving files right now; try again when that transfer finishes")));
            }
            self.seal_open()?;
        }
        Ok(())
    }

    pub fn trash_list(&self) -> Result<Vec<Entry>> {
        cat::children(self.cat.conn(), cat::TRASH_ID)
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<(VPath, Entry)>> {
        cat::search(self.cat.conn(), query, limit)
    }

    pub fn pack_path(&self, name: &str) -> PathBuf {
        self.root.join(PACKS_DIR).join(name)
    }

    // -----------------------------------------------------------------------
    // Pack lifecycle
    // -----------------------------------------------------------------------

    /// Make sure `job` has an open pack for `project` in hand, resuming or
    /// creating one. A pack another live session is writing to (it holds the
    /// file lock) is left alone, and this session starts a new pack.
    fn ensure_pack(&mut self, job: &str, project: &str) -> Result<()> {
        if self.open.as_ref().is_some_and(|p| p.job == job && p.project == project) {
            return Ok(());
        }
        self.release_open()?;
        if let Some(p) = cat::open_pack_for(self.cat.conn(), job, project)? {
            if self.adopt_pack(&p)? {
                return Ok(());
            }
        }
        let conn = self.cat.conn();
        conn.execute(
            "INSERT INTO packs(name, job, state, bytes, created, project) VALUES(?1, ?2, 'open', 0, ?3, ?4)",
            params![format!("pending-{}", util::random_hex(8)), job, now_secs(), project],
        )?;
        let id = conn.last_insert_rowid();
        let name = format!("{}/pack-{id:08}.tar", util::year_month(now_secs()));
        conn.execute("UPDATE packs SET name=?1 WHERE id=?2", params![name, id])?;
        let p = cat::pack(conn, id)?;
        if !self.adopt_pack(&p)? {
            return Err(Error::other(format!("new pack {name} is locked by another process")));
        }
        Ok(())
    }

    /// Open an existing open pack for writing: lock it, truncate it to its
    /// committed length, and write its header if it is new. Returns false if
    /// another process holds its lock.
    fn adopt_pack(&mut self, pack: &crate::catalog::Pack) -> Result<bool> {
        let path = self.pack_path(&pack.name);
        fs::create_dir_all(path.parent().unwrap())?;
        let committed = pack.bytes;
        let file = OpenOptions::new().read(true).write(true).create(committed == 0).truncate(false).open(&path)?;
        if !util::try_lock(&file)? {
            return Ok(false);
        }
        let len = file.metadata()?.len();
        if len < committed {
            return Err(Error::Corrupt(format!(
                "pack {} is {len} bytes but the catalog committed {committed}; it may be damaged",
                pack.name
            )));
        }
        // Discard anything written after the last commit (an interrupted write).
        file.set_len(committed)?;
        file.sync_all()?;
        util::sync_dir(path.parent().unwrap())?;
        let mut w = BufWriter::with_capacity(1 << 20, file);
        w.seek(SeekFrom::Start(committed))?;
        let conn = self.cat.conn();
        let solid_seq: u32 =
            conn.query_row("SELECT COUNT(*) FROM blobs WHERE pack_id=?1 AND kind='s'", [pack.id], |r| r.get::<_, i64>(0))? as u32;
        let job = pack.job.clone().unwrap_or_default();
        let project = pack.project.clone().unwrap_or_default();
        let mut open = OpenPack { job, project, id: pack.id, path, tar: TarWriter::new(w, committed), solid_seq };
        if committed == 0 {
            let header = PackHeader {
                format: FORMAT_VERSION,
                archive_id: &self.cfg.archive_id,
                pack: &pack.name,
                created: now_secs(),
                project: project_ref(conn, &open.project)?,
                note: "See .archive/RECOVERY.txt at the end of this pack for recovery instructions.",
            };
            open.tar.append(".archive/PACK.json", 0o644, now_secs(), &serde_json::to_vec_pretty(&header)?)?;
            sync(&mut open.tar)?;
            conn.execute("UPDATE packs SET bytes=?1 WHERE id=?2", params![open.tar.position() as i64, pack.id])?;
        }
        self.open = Some(open);
        Ok(true)
    }

    /// Close the in-hand pack without sealing it (it stays resumable).
    fn release_open(&mut self) -> Result<()> {
        self.settle_pending()?;
        if let Some(mut p) = self.open.take() {
            p.tar.flush()?;
        }
        Ok(())
    }

    /// Truncate the open pack back to `to`, discarding buffered bytes.
    fn rollback(&mut self, to: u64) -> Result<()> {
        let p = self.open.as_mut().expect("rollback without open pack");
        // A duplicate descriptor shares the open file, so the pack lock is kept.
        let fresh = p.tar.get_mut().get_ref().try_clone()?;
        let old = std::mem::replace(&mut p.tar, TarWriter::new(BufWriter::with_capacity(1 << 20, fresh), to));
        // into_parts drops the unflushed buffer instead of writing it.
        let (old_file, _discarded) = old.into_inner().into_parts();
        drop(old_file);
        let f = p.tar.get_mut().get_mut();
        f.set_len(to)?;
        f.seek(SeekFrom::Start(to))?;
        f.sync_all()?;
        Ok(())
    }

    /// Stream a member into the open pack and make it durable.
    /// Returns (member start, data offset, data length).
    fn write_member(&mut self, member: &str, mode: u32, mtime_ns: i64, payload: &mut dyn Read) -> Result<(u64, u64, u64)> {
        let p = self.open.as_mut().expect("write without open pack");
        let start = p.tar.position();
        let res = (|| -> io::Result<(u64, u64)> {
            let m = p.tar.begin_member(member, Kind::File, mode, mtime_ns.div_euclid(1_000_000_000), None)?;
            let data_off = m.data_pos;
            io::copy(payload, &mut p.tar)?;
            let len = p.tar.end_member(m)?;
            sync(&mut p.tar)?;
            util::drop_cache(p.tar.get_mut().get_ref(), data_off, len);
            Ok((data_off, len))
        })();
        match res {
            Ok((off, len)) => Ok((start, off, len)),
            Err(e) => {
                self.rollback(start)?;
                Err(e.into())
            }
        }
    }

    /// Record folders in the project's pack (for its manifest and for tar).
    fn mkdir_entries(&mut self, job: &str, project: &str, dirs: &[&DirSpec]) -> Result<()> {
        self.ensure_pack(job, project)?;
        let p = self.open.as_mut().unwrap();
        let start = p.tar.position();
        let written = (|| -> io::Result<()> {
            for d in dirs {
                p.tar.append_dir(&d.path.to_rel_string(), d.mode, d.mtime_ns.div_euclid(1_000_000_000))?;
            }
            sync(&mut p.tar)
        })();
        if let Err(e) = written {
            self.rollback(start)?;
            return Err(e.into());
        }
        let p = self.open.as_ref().unwrap();
        let (pack_id, end) = (p.id, p.tar.position());
        let tx = self.cat.conn_mut().transaction()?;
        let res = (|| -> Result<()> {
            for d in dirs {
                cat::insert_pack_entry(
                    &tx,
                    pack_id,
                    &PackEntry {
                        path: d.path.to_rel_string(),
                        kind: NodeKind::Dir,
                        sha256: None,
                        size: 0,
                        mtime_ns: d.mtime_ns,
                        mode: d.mode,
                        member: None,
                        offset: None,
                        target: None,
                        same_as: None,
                    },
                    None,
                )?;
            }
            Self::commit_bytes(&tx, pack_id, end)
        })();
        match res {
            Ok(()) => {
                tx.commit()?;
                Ok(())
            }
            Err(e) => {
                drop(tx);
                self.rollback(start)?;
                Err(e)
            }
        }
    }

    fn commit_bytes(c: &Connection, pack_id: i64, bytes: u64) -> Result<()> {
        c.execute("UPDATE packs SET bytes=?1 WHERE id=?2", params![bytes as i64, pack_id])?;
        Ok(())
    }

    fn maybe_rotate(&mut self) -> Result<()> {
        if self.open.as_ref().is_some_and(|p| p.tar.position() >= self.cfg.pack_target_bytes) {
            self.seal_open()?;
        }
        Ok(())
    }

    /// Append the manifest and recovery files, write the end marker, and mark sealed.
    fn seal_open(&mut self) -> Result<()> {
        self.settle_pending()?;
        let Some(mut p) = self.open.take() else { return Ok(()) };
        let conn = self.cat.conn();
        let pack = cat::pack(conn, p.id)?;
        let manifest = Manifest {
            format: FORMAT_VERSION,
            archive_id: self.cfg.archive_id.clone(),
            pack: pack.name.clone(),
            sealed_at: now_secs(),
            project: project_ref(conn, &p.project)?,
            entries: cat::pack_entries(conn, p.id)?,
        };
        let now = now_secs();
        // Deduplicated files as plain "copy<TAB>stored-copy<TAB>sha256<TAB>mtime"
        // lines, so recover.sh can recreate them with only standard tools. The
        // copy's own modification time is in `touch -t` form (UTC), since
        // copying the stored twin brings that file's time instead.
        let dups: String = manifest
            .entries
            .iter()
            .filter_map(|e| {
                let src = e.same_as.as_deref()?;
                let clean = |s: &str| !s.contains(['\t', '\n', '\r']);
                (clean(&e.path) && clean(src))
                    .then(|| {
                        let sha = e.sha256.map(|d| d.to_hex()).unwrap_or_default();
                        format!("{}\t{}\t{sha}\t{}\n", e.path, src, util::touch_stamp_utc(e.mtime_ns.div_euclid(1_000_000_000)))
                    })
            })
            .collect();
        p.tar.append(".archive/MANIFEST.json", 0o644, now, &serde_json::to_vec_pretty(&manifest)?)?;
        if !dups.is_empty() {
            p.tar.append(".archive/DUPLICATES.tsv", 0o644, now, dups.as_bytes())?;
        }
        p.tar.append(".archive/RECOVERY.txt", 0o644, now, recovery::RECOVERY_TXT.as_bytes())?;
        p.tar.append(".archive/recover.sh", 0o755, now, recovery::RECOVER_SH.as_bytes())?;
        p.tar.finish()?;
        sync(&mut p.tar)?;
        let tx = self.cat.conn_mut().transaction()?;
        Self::commit_bytes(&tx, p.id, p.tar.position())?;
        cat::set_pack_state(&tx, p.id, PackState::Sealed)?;
        tx.commit()?;
        Ok(())
    }

    /// Seal the open packs of `job` (one per project it wrote to). Packs
    /// another live session holds are left for it.
    pub fn seal_job(&mut self, job: &str) -> Result<()> {
        if self.open.as_ref().is_some_and(|p| p.job == job) {
            self.seal_open()?;
        }
        for p in cat::open_packs_of_job(self.cat.conn(), job)? {
            self.release_open()?;
            if self.adopt_pack(&p)? {
                self.seal_open()?;
            }
        }
        Ok(())
    }

    /// Seal open packs left behind by interrupted transfers. Packs a live
    /// session is writing to are skipped. With `older_than_secs`, only packs
    /// untouched for that long are sealed.
    pub fn seal_all_open(&mut self, older_than_secs: Option<i64>) -> Result<usize> {
        self.release_open()?;
        let open = cat::packs_in_state(self.cat.conn(), PackState::Open)?;
        let mut n = 0;
        for p in open {
            if let Some(age) = older_than_secs {
                // The pack file's mtime is the last time the job wrote to it.
                let idle = fs::metadata(self.pack_path(&p.name))
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(i64::MAX);
                if idle < age {
                    continue;
                }
            }
            if self.adopt_pack(&p)? {
                self.seal_open()?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Which stored zstd frames hold a file's bytes, for sending them as-is.
    pub fn frame_plan(&self, content_id: i64) -> Result<FramePlan> {
        let loc = cat::location(self.cat.conn(), content_id)?;
        let path = self.pack_path(&loc.pack_name);
        let mut f = File::open(&path)?;
        let table = crate::seekable::SeekTable::read(&mut f, loc.data_off, loc.data_len)?;
        let frames = if loc.size == 0 {
            Vec::new()
        } else {
            let first = table.frame_at(loc.blob_off).ok_or_else(|| Error::Corrupt("file offset past its block".into()))?;
            let last = table.frame_at(loc.blob_off + loc.size - 1).ok_or_else(|| Error::Corrupt("file runs past its block".into()))?;
            table.frames[first..=last].to_vec()
        };
        let skip = frames.first().map(|fr| loc.blob_off - fr.d_off).unwrap_or(0);
        Ok(FramePlan { file: f, start: loc.data_off, frames, skip, len: loc.size })
    }

    // -----------------------------------------------------------------------
    // Placing files in the tree
    // -----------------------------------------------------------------------

    /// Whether a put to `dest` can proceed; `Some` is the final answer (e.g.
    /// skipped) so the sender needn't send the bytes.
    pub fn precheck_put(&self, dest: &VPath, policy: Conflict, job: &str) -> Result<Option<PutOutcome>> {
        Self::precheck(self.cat.conn(), dest, policy, job)
    }

    /// Decide up front whether a put can proceed, so skipped files don't cost a write.
    fn precheck(c: &Connection, dest: &VPath, policy: Conflict, job: &str) -> Result<Option<PutOutcome>> {
        let name = dest.name().ok_or_else(|| Error::InvalidPath("cannot write to the archive root".into()))?;
        let parent = dest.parent().unwrap_or_default();
        let parent_entry = match cat::resolve(c, &parent)? {
            Some(e) if e.kind != NodeKind::Dir => return Err(Error::NotADirectory(parent.to_string())),
            Some(e) => Some(e),
            None => {
                // Missing parents get created; fail now if a file blocks the way.
                let mut cur = VPath::root();
                for comp in parent.components() {
                    cur = cur.join(comp)?;
                    if let Some(e) = cat::resolve(c, &cur)? {
                        if e.kind != NodeKind::Dir {
                            return Err(Error::NotADirectory(cur.to_string()));
                        }
                    } else {
                        break;
                    }
                }
                None
            }
        };
        project_of_dest(c, dest)?;
        if let Some(pe) = parent_entry {
            if let Some(existing) = cat::child(c, pe.id, name)? {
                match policy {
                    Conflict::Skip => return Ok(Some(PutOutcome { dest: dest.clone(), status: PutStatus::Skipped })),
                    Conflict::Fail => return Err(Error::AlreadyExists(dest.to_string())),
                    Conflict::Replace if existing.kind == NodeKind::Dir => {
                        return Err(Error::AlreadyExists(format!("{dest} is a folder")));
                    }
                    Conflict::Replace if !stored_by_this_job(c, &existing, job)? => return Err(frozen(dest)),
                    _ => {}
                }
            }
        }
        Ok(None)
    }

    /// Reference content already stored in the project at `req.dest` (deduplication).
    fn link_in(c: &Connection, pack_id: i64, project: &str, req: &PutFile, sha256: &Digest, job: &str) -> Result<PutOutcome> {
        if let Some(out) = Self::precheck(c, &req.dest, req.policy, job)? {
            // Skip policy: identical content at the destination still counts as archived.
            if let Some(e) = cat::resolve(c, &req.dest)? {
                if e.sha256.as_ref() == Some(sha256) {
                    return Ok(PutOutcome { dest: req.dest.clone(), status: PutStatus::Identical });
                }
            }
            return Ok(out);
        }
        let (content_id, size) = cat::content_by_hash(c, project, sha256)?
            .ok_or_else(|| Error::NotFound(format!("content {sha256} is not stored in this project")))?;
        if size != req.meta.size {
            return Err(Error::Verify(format!("size mismatch for {}: stored {size}, sent {}", req.dest, req.meta.size)));
        }
        let out = Self::place(c, &req.dest, content_id, sha256, &req.meta, req.policy, PutStatus::Deduplicated, job)?;
        if out.status == PutStatus::Deduplicated {
            let same_as = cat::stored_path(c, project, sha256)?;
            cat::insert_pack_entry(
                c,
                pack_id,
                &PackEntry {
                    path: out.dest.to_rel_string(),
                    kind: NodeKind::File,
                    sha256: Some(*sha256),
                    size,
                    mtime_ns: req.meta.mtime_ns,
                    mode: req.meta.mode,
                    member: None,
                    offset: None,
                    target: None,
                    same_as,
                },
                None,
            )?;
        }
        Ok(out)
    }

    /// Insert a file node for `content_id` at `dest`, applying the conflict policy.
    #[allow(clippy::too_many_arguments)]
    fn place(
        c: &Connection,
        dest: &VPath,
        content_id: i64,
        sha: &Digest,
        meta: &FileMeta,
        policy: Conflict,
        status: PutStatus,
        job: &str,
    ) -> Result<PutOutcome> {
        let parent = cat::mkdir_p(c, &dest.parent().unwrap_or_default())?;
        let name = dest.name().ok_or_else(|| Error::InvalidPath("cannot write to the archive root".into()))?;
        let mut final_name = name.to_string();
        if let Some(existing) = cat::child(c, parent, name)? {
            if existing.kind == NodeKind::File && existing.sha256.as_ref() == Some(sha) {
                return Ok(PutOutcome { dest: dest.clone(), status: PutStatus::Identical });
            }
            match policy {
                Conflict::Skip => return Ok(PutOutcome { dest: dest.clone(), status: PutStatus::Skipped }),
                Conflict::Fail => return Err(Error::AlreadyExists(dest.to_string())),
                Conflict::Replace => {
                    if existing.kind == NodeKind::Dir {
                        return Err(Error::AlreadyExists(format!("{dest} is a folder")));
                    }
                    if !stored_by_this_job(c, &existing, job)? {
                        return Err(frozen(dest));
                    }
                    cat::trash(c, existing.id)?;
                }
                Conflict::KeepBoth => {
                    let mut n = 2;
                    loop {
                        let cand = util::numbered_name(name, n);
                        if cat::child(c, parent, &cand)?.is_none() {
                            final_name = cand;
                            break;
                        }
                        n += 1;
                    }
                }
            }
        }
        cat::insert_file(c, &NewFile { parent, name: &final_name, content_id, size: meta.size, mtime_ns: meta.mtime_ns, mode: meta.mode })?;
        let dest = dest.parent().unwrap_or_default().join(&final_name)?;
        Ok(PutOutcome { dest, status })
    }

    fn content_for(c: &Connection, project: &str, sha: &Digest, size: u64, blob_id: i64, blob_off: u64) -> Result<i64> {
        match cat::content_by_hash(c, project, sha)? {
            // Stored concurrently by someone else: reuse it (the new blob becomes garbage).
            Some((id, _)) => Ok(id),
            None => cat::insert_content(c, project, sha, size, blob_id, blob_off),
        }
    }

    // -----------------------------------------------------------------------
    // Reading
    // -----------------------------------------------------------------------

    pub fn open_content(&self, content_id: i64) -> Result<RangeReader<File>> {
        let loc = cat::location(self.cat.conn(), content_id)?;
        let f = File::open(self.pack_path(&loc.pack_name))?;
        RangeReader::with_cache(f, loc.data_off, loc.data_len, loc.blob_off, loc.size, Some((self.cache.clone(), loc.pack_id as u64)))
    }

    /// Before writing another file: settle everything if the next write goes
    /// to another pack (or this one is full), or else the oldest check if as
    /// many are running as allowed.
    fn make_room(&mut self, job: &str, project: &str) -> Result<()> {
        let switching = self.open.as_ref().is_some_and(|p| p.job != job || p.project != project);
        let full = self.open.as_ref().is_some_and(|p| p.tar.position() >= self.cfg.pack_target_bytes);
        if switching || full {
            self.settle_pending()?;
            return self.maybe_rotate();
        }
        while self.pending.len() >= MAX_PENDING {
            self.settle_one()?;
        }
        Ok(())
    }

    /// Wait for every running check, committing each file that passed.
    fn settle_pending(&mut self) -> Result<()> {
        while !self.pending.is_empty() {
            self.settle_one()?;
        }
        Ok(())
    }

    /// Wait for the oldest running check. If it passed, commit its file;
    /// if not, discard it and everything written after it, and note them
    /// for `settle` (the sender sends them again).
    fn settle_one(&mut self) -> Result<()> {
        let Some(p) = self.pending.pop_front() else { return Ok(()) };
        let waited = Instant::now();
        let (checked, took) =
            p.check.join().unwrap_or_else(|_| (Err(Error::Verify("the read-back check stopped unexpectedly".into())), Duration::ZERO));
        self.timing.wait += waited.elapsed();
        self.timing.check += took;
        let t = Instant::now();
        let committed = checked.and_then(|c| self.commit_put(&p.put, c));
        self.timing.commit += t.elapsed();
        if let Err(e) = committed {
            self.rollback(p.start)?;
            let reason = e.to_string();
            self.failed_checks.extend(p.put.dests().into_iter().map(|d| (d, reason.clone())));
            for later in std::mem::take(&mut self.pending) {
                let why = "written after a file whose check failed";
                self.failed_checks.extend(later.put.dests().into_iter().map(|d| (d, why.to_string())));
            }
        }
        Ok(())
    }

    /// Record a checked file (or block) in the catalog.
    fn commit_put(&mut self, put: &PendingPut, checked: Checked) -> Result<()> {
        let tx = self.cat.conn_mut().transaction()?;
        match (put, checked) {
            (PendingPut::File { job, project, req, member, data_off, len, sent, pack_id, end }, Checked::File { digest, raw_len }) => {
                if digest != *sent {
                    return Err(Error::Verify(format!("{}: checksum mismatch after writing (sent {sent}, stored {digest})", req.dest)));
                }
                if raw_len != req.meta.size {
                    return Err(Error::Verify(format!("{}: size changed while sending ({} expected, {raw_len} received)", req.dest, req.meta.size)));
                }
                let blob = cat::insert_blob(&tx, *pack_id, member, "f", *data_off, *len, raw_len)?;
                let content = Self::content_for(&tx, project, sent, raw_len, blob, 0)?;
                let out = Self::place(&tx, &req.dest, content, sent, &req.meta, req.policy, PutStatus::Stored, job)?;
                cat::insert_pack_entry(
                    &tx,
                    *pack_id,
                    &PackEntry {
                        path: out.dest.to_rel_string(),
                        kind: NodeKind::File,
                        sha256: Some(*sent),
                        size: raw_len,
                        mtime_ns: req.meta.mtime_ns,
                        mode: req.meta.mode,
                        member: Some(member.clone()),
                        offset: None,
                        target: None,
                        same_as: None,
                    },
                    Some(blob),
                )?;
                Self::commit_bytes(&tx, *pack_id, *end)?;
                self.timing.files += 1;
                self.timing.bytes += raw_len;
            }
            (PendingPut::Solid { job, project, req, pre, member, data_off, len, pack_id, end }, Checked::Solid { raw_len, offsets }) => {
                let blob = cat::insert_blob(&tx, *pack_id, member, "s", *data_off, *len, raw_len)?;
                for ((m, off), pre) in req.members.iter().zip(&offsets).zip(pre) {
                    if pre.is_some() {
                        continue;
                    }
                    let content = Self::content_for(&tx, project, &m.sha256, m.meta.size, blob, *off)?;
                    let out = Self::place(&tx, &m.dest, content, &m.sha256, &m.meta, req.policy, PutStatus::Stored, job)?;
                    if out.status == PutStatus::Stored {
                        cat::insert_pack_entry(
                            &tx,
                            *pack_id,
                            &PackEntry {
                                path: out.dest.to_rel_string(),
                                kind: NodeKind::File,
                                sha256: Some(m.sha256),
                                size: m.meta.size,
                                mtime_ns: m.meta.mtime_ns,
                                mode: m.meta.mode,
                                member: Some(member.clone()),
                                offset: Some(*off),
                                target: None,
                                same_as: None,
                            },
                            Some(blob),
                        )?;
                    }
                    self.timing.files += 1;
                }
                Self::commit_bytes(&tx, *pack_id, *end)?;
                self.timing.bytes += raw_len;
            }
            _ => return Err(Error::Verify("internal error: a check doesn't match its file".into())),
        }
        tx.commit()?;
        Ok(())
    }

    /// Log where a finished job's time went on this side, then start over.
    fn log_timing(&mut self, job: &str) {
        let t = std::mem::take(&mut self.timing);
        if t.files == 0 {
            return;
        }
        let secs = |d: Duration| d.as_secs_f64();
        util::log_timing(&format!(
            "datahold job={job} files={} bytes={} receive_s={:.1} check_s={:.1} wait_for_checks_s={:.1} commit_s={:.1}",
            t.files,
            t.bytes,
            secs(t.receive),
            secs(t.check),
            secs(t.wait),
            secs(t.commit)
        ));
    }

    fn verify_stream(path: &Path, data_off: u64, len: u64) -> Result<(Digest, u64)> {
        let f = File::open(path)?;
        let mut rr = RangeReader::whole(f, data_off, len)?;
        hash_reader(&mut rr).map_err(|e| Error::Verify(format!("read-back failed: {e}")))
    }
}

fn sync(tar: &mut TarWriter<BufWriter<File>>) -> io::Result<()> {
    tar.flush()?;
    tar.get_mut().get_ref().sync_data()
}

impl Archive for Store {
    fn stat(&mut self, path: &VPath) -> Result<Option<Entry>> {
        self.settle_pending()?;
        let mut e = cat::resolve(self.cat.conn(), path)?;
        if let Some(e) = e.as_mut() {
            if let Some(p) = &e.project {
                e.project_path = project_ref(self.cat.conn(), p)?.map(|r| format!("/{}", r.path));
            }
        }
        Ok(e)
    }

    fn list(&mut self, path: &VPath) -> Result<Vec<Entry>> {
        self.settle_pending()?;
        let dir = cat::resolve_dir(self.cat.conn(), path)?;
        cat::children(self.cat.conn(), dir.id)
    }

    fn walk(&mut self, path: &VPath) -> Result<Vec<(String, Entry)>> {
        self.settle_pending()?;
        let dir = cat::resolve_dir(self.cat.conn(), path)?;
        cat::walk(self.cat.conn(), dir.id)
    }

    fn sizes_present(&mut self, scope: &VPath, sizes: &[u64]) -> Result<Vec<bool>> {
        self.settle_pending()?;
        let Some(project) = cat::project_for(self.cat.conn(), scope)? else { return Ok(vec![false; sizes.len()]) };
        sizes.iter().map(|&s| cat::size_present(self.cat.conn(), &project, s)).collect()
    }

    fn have(&mut self, scope: &VPath, hashes: &[Digest]) -> Result<Vec<bool>> {
        self.settle_pending()?;
        let Some(project) = cat::project_for(self.cat.conn(), scope)? else { return Ok(vec![false; hashes.len()]) };
        hashes.iter().map(|h| Ok(cat::content_by_hash(self.cat.conn(), &project, h)?.is_some())).collect()
    }

    fn mkdirs(&mut self, job: &str, dirs: &[DirSpec]) -> Result<()> {
        self.settle_pending()?;
        let dirs: Vec<&DirSpec> = dirs.iter().filter(|d| !d.path.is_root()).collect();
        if dirs.is_empty() {
            return Ok(());
        }
        // The folders first. One marked as a project's top becomes a new
        // project, unless it lands inside a project (then it's added to it).
        let mut groups: Vec<(String, Vec<&DirSpec>)> = Vec::new();
        {
            let tx = self.cat.conn_mut().transaction()?;
            for d in &dirs {
                let parent_path = d.path.parent().unwrap_or_default();
                let name = d.path.name().unwrap_or_default();
                let project = match cat::project_for(&tx, &parent_path)? {
                    None if d.project => {
                        let parent = cat::mkdir_p(&tx, &parent_path)?;
                        cat::create_project(&tx, parent, name, d.mtime_ns, d.mode)?.1
                    }
                    None => return Err(outside_projects(&d.path)),
                    Some(project) => {
                        let parent = cat::mkdir_p(&tx, &parent_path)?;
                        cat::mkdir(&tx, parent, name, d.mtime_ns, d.mode)?;
                        project
                    }
                };
                match groups.last_mut() {
                    Some((p, v)) if *p == project => v.push(d),
                    _ => groups.push((project, vec![d])),
                }
            }
            tx.commit()?;
        }
        // Then their entries, in each project's pack.
        for (project, dirs) in groups {
            self.mkdir_entries(job, &project, &dirs)?;
        }
        Ok(())
    }
    fn symlink(&mut self, job: &str, dest: &VPath, target: &str, mtime_ns: i64, policy: Conflict) -> Result<PutOutcome> {
        self.settle_pending()?;
        if let Some(existing) = cat::resolve(self.cat.conn(), dest)? {
            if existing.kind == NodeKind::Symlink && existing.target.as_deref() == Some(target) {
                return Ok(PutOutcome { dest: dest.clone(), status: PutStatus::Identical });
            }
        }
        if let Some(out) = Self::precheck(self.cat.conn(), dest, policy, job)? {
            return Ok(out);
        }
        let project = project_of_dest(self.cat.conn(), dest)?;
        self.ensure_pack(job, &project)?;
        let p = self.open.as_mut().unwrap();
        let start = p.tar.position();
        p.tar.append_symlink(&dest.to_rel_string(), target, mtime_ns.div_euclid(1_000_000_000))?;
        sync(&mut p.tar)?;
        let (pack_id, end) = (p.id, p.tar.position());
        let tx = self.cat.conn_mut().transaction()?;
        let res = (|| -> Result<PutOutcome> {
            let parent = cat::mkdir_p(&tx, &dest.parent().unwrap_or_default())?;
            let name = dest.name().unwrap();
            let mut final_name = name.to_string();
            if cat::child(&tx, parent, name)?.is_some() {
                match policy {
                    Conflict::KeepBoth => {
                        let mut n = 2;
                        while cat::child(&tx, parent, &util::numbered_name(name, n))?.is_some() {
                            n += 1;
                        }
                        final_name = util::numbered_name(name, n);
                    }
                    _ => return Err(Error::AlreadyExists(dest.to_string())),
                }
            }
            cat::insert_symlink(&tx, parent, &final_name, target, mtime_ns)?;
            let final_dest = dest.parent().unwrap_or_default().join(&final_name)?;
            cat::insert_pack_entry(
                &tx,
                pack_id,
                &PackEntry {
                    path: final_dest.to_rel_string(),
                    kind: NodeKind::Symlink,
                    sha256: None,
                    size: 0,
                    mtime_ns,
                    mode: 0o777,
                    member: None,
                    offset: None,
                    target: Some(target.to_string()),
                    same_as: None,
                },
                None,
            )?;
            Self::commit_bytes(&tx, pack_id, end)?;
            Ok(PutOutcome { dest: final_dest, status: PutStatus::Stored })
        })();
        match res {
            Ok(out) => {
                tx.commit()?;
                Ok(out)
            }
            Err(e) => {
                drop(tx);
                self.rollback(start)?;
                Err(e)
            }
        }
    }

    fn link(&mut self, job: &str, req: &PutFile, sha256: &Digest) -> Result<PutOutcome> {
        self.settle_pending()?;
        self.link_many(job, &[(req.clone(), *sha256)])?.pop().expect("one result per item")
    }

    fn link_many(&mut self, job: &str, items: &[(PutFile, Digest)]) -> Result<Vec<Result<PutOutcome>>> {
        self.settle_pending()?;
        let mut out: Vec<Option<Result<PutOutcome>>> = (0..items.len()).map(|_| None).collect();
        // Each project's references go in that project's pack.
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for (i, (req, _)) in items.iter().enumerate() {
            match project_of_dest(self.cat.conn(), &req.dest) {
                Ok(p) => match groups.iter_mut().find(|(g, _)| *g == p) {
                    Some((_, v)) => v.push(i),
                    None => groups.push((p, vec![i])),
                },
                Err(e) => out[i] = Some(Err(e)),
            }
        }
        for (project, idx) in groups {
            self.ensure_pack(job, &project)?;
            let pack_id = self.open.as_ref().unwrap().id;
            let mut tx = self.cat.conn_mut().transaction()?;
            for i in idx {
                let (req, sha) = &items[i];
                // A savepoint per item: one failure doesn't undo the others.
                let sp = tx.savepoint()?;
                let res = Self::link_in(&sp, pack_id, &project, req, sha, job);
                if res.is_ok() {
                    sp.commit()?;
                }
                out[i] = Some(res);
            }
            tx.commit()?;
        }
        Ok(out.into_iter().map(|r| r.expect("a result for every item")).collect())
    }

    fn put_file(&mut self, job: &str, req: &PutFile, payload: &mut dyn Payload) -> Result<PutOutcome> {
        if let Some(out) = Self::precheck(self.cat.conn(), &req.dest, req.policy, job)? {
            return Ok(out);
        }
        let project = project_of_dest(self.cat.conn(), &req.dest)?;
        self.make_room(job, &project)?;
        self.ensure_pack(job, &project)?;
        let member = format!("{}.zst", req.dest.to_rel_string());
        let t = Instant::now();
        let (start, data_off, len) = self.write_member(&member, req.meta.mode, req.meta.mtime_ns, payload)?;
        self.timing.receive += t.elapsed();
        let Some(sent) = payload.digest() else {
            self.rollback(start)?;
            return Err(Error::Verify("sender did not finish the stream".into()));
        };
        let p = self.open.as_ref().unwrap();
        let (pack_id, end, path) = (p.id, p.tar.position(), p.path.clone());
        // Read it back on another thread; `commit_put` records it once that passes.
        let check = std::thread::spawn(move || {
            let t = Instant::now();
            let r = Self::verify_stream(&path, data_off, len).map(|(digest, raw_len)| Checked::File { digest, raw_len });
            (r, t.elapsed())
        });
        let put = PendingPut::File { job: job.to_string(), project, req: req.clone(), member, data_off, len, sent, pack_id, end };
        self.pending.push_back(Pending { start, check, put });
        Ok(PutOutcome { dest: req.dest.clone(), status: PutStatus::Stored })
    }

    fn put_solid(&mut self, job: &str, req: &PutSolid, payload: &mut dyn Payload) -> Result<Vec<PutOutcome>> {
        if req.members.is_empty() {
            return Ok(Vec::new());
        }
        let mut pre = Vec::with_capacity(req.members.len());
        for m in &req.members {
            pre.push(Self::precheck(self.cat.conn(), &m.dest, req.policy, job)?);
        }
        let project = project_of_dest(self.cat.conn(), &req.members[0].dest)?;
        for m in &req.members[1..] {
            if project_of_dest(self.cat.conn(), &m.dest)? != project {
                return Err(Error::InvalidPath("a block of small files must all go into one project".into()));
            }
        }
        self.make_room(job, &project)?;
        self.ensure_pack(job, &project)?;
        let p = self.open.as_mut().unwrap();
        p.solid_seq += 1;
        let member = format!(".archive/solid-{:08}-{:05}.tar.zst", p.id, p.solid_seq);
        let t = Instant::now();
        let (start, data_off, len) = self.write_member(&member, 0o644, util::now_ns(), payload)?;
        self.timing.receive += t.elapsed();
        let Some(sent) = payload.digest() else {
            self.rollback(start)?;
            return Err(Error::Verify("sender did not finish the stream".into()));
        };
        let p = self.open.as_ref().unwrap();
        let (pack_id, end, path) = (p.id, p.tar.position(), p.path.clone());

        // Read back, decompress, and check the block and every file inside it, on another thread.
        let members = req.members.clone();
        let check = std::thread::spawn(move || {
            let t = Instant::now();
            let r = (|| -> Result<Checked> {
                let f = File::open(&path)?;
                let mut raw = Vec::new();
                RangeReader::whole(f, data_off, len)?.read_to_end(&mut raw).map_err(|e| Error::Verify(format!("read-back failed: {e}")))?;
                if Digest::of(&raw) != sent {
                    return Err(Error::Verify("small-file block checksum mismatch after writing".into()));
                }
                let entries = tar::list(&mut Cursor::new(&raw), 0, None)?;
                if entries.len() != members.len() {
                    return Err(Error::Verify(format!("block holds {} files, {} declared", entries.len(), members.len())));
                }
                let mut offsets = Vec::with_capacity(entries.len());
                for (e, m) in entries.iter().zip(&members) {
                    let data = &raw[e.data_off as usize..(e.data_off + e.size) as usize];
                    if e.path != m.dest.to_rel_string() || e.size != m.meta.size || Digest::of(data) != m.sha256 {
                        return Err(Error::Verify(format!("{}: does not match its checksum", m.dest)));
                    }
                    offsets.push(e.data_off);
                }
                Ok(Checked::Solid { raw_len: raw.len() as u64, offsets })
            })();
            (r, t.elapsed())
        });
        let outs = req
            .members
            .iter()
            .zip(&pre)
            .map(|(m, p)| p.clone().unwrap_or(PutOutcome { dest: m.dest.clone(), status: PutStatus::Stored }))
            .collect();
        let put = PendingPut::Solid { job: job.to_string(), project, req: req.clone(), pre, member, data_off, len, pack_id, end };
        self.pending.push_back(Pending { start, check, put });
        Ok(outs)
    }

    fn settle(&mut self) -> Result<Vec<(VPath, String)>> {
        self.settle_pending()?;
        Ok(std::mem::take(&mut self.failed_checks))
    }

    fn finish_job(&mut self, job: &str) -> Result<()> {
        self.seal_job(job)?;
        self.log_timing(job);
        let failed = std::mem::take(&mut self.failed_checks);
        if let Some((dest, why)) = failed.first() {
            return Err(Error::Verify(format!(
                "{} file(s) failed their check after writing and weren't kept (first: {dest}: {why}). Send again to retry them.",
                failed.len()
            )));
        }
        Ok(())
    }

    fn read_file(&mut self, path: &VPath) -> Result<(Entry, Box<dyn Read + Send + '_>)> {
        self.settle_pending()?;
        let e = cat::resolve(self.cat.conn(), path)?.ok_or_else(|| Error::NotFound(path.to_string()))?;
        let cid = match (e.kind, e.content_id) {
            (NodeKind::File, Some(cid)) => cid,
            _ => return Err(Error::InvalidPath(format!("{path} is not a file"))),
        };
        let r = self.open_content(cid)?;
        Ok((e, Box::new(r)))
    }

    fn info(&mut self, path: &VPath) -> Result<crate::maint::Info> {
        self.settle_pending()?;
        crate::maint::info(self, path)
    }

    fn search(&mut self, query: &str, limit: usize) -> Result<Vec<(VPath, Entry)>> {
        self.settle_pending()?;
        Store::search(self, query, limit)
    }

    fn create_folder(&mut self, path: &VPath) -> Result<()> {
        self.settle_pending()?;
        self.mkdir_p(path)
    }

    fn rename(&mut self, from: &VPath, to: &VPath) -> Result<()> {
        self.settle_pending()?;
        Store::rename(self, from, to)
    }

    fn trash(&mut self, path: &VPath) -> Result<i64> {
        self.settle_pending()?;
        Store::trash(self, path)
    }

    fn restore(&mut self, trash_id: i64, to: Option<&VPath>) -> Result<VPath> {
        self.settle_pending()?;
        Store::restore(self, trash_id, to)
    }

    fn trash_list(&mut self) -> Result<Vec<Entry>> {
        self.settle_pending()?;
        Store::trash_list(self)
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.release_open();
    }
}
