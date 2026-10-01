//! One background maintenance run over an archive. A lock file keeps runs
//! from overlapping.
//!
//! Each run: seal packs abandoned by interrupted uploads, protect sealed packs
//! with PAR2, check packs not verified for `scrub_every_days` (within a time
//! budget, oldest first), and once a day purge old trash, snapshot the
//! catalog, and rewrite `INDEX.tsv` (the plain-text list of every file, for
//! finding things without this software). The result is written to
//! `worker-status.json` for the app.
//!
//! Runs start after every upload and when the desktop app first connects to
//! an archive in a session; an hourly cron entry is optional. The 90-day check
//! interval assumes the pool is also scrubbed by its filesystem (ZFS).

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::catalog::{self as cat, PackState};
use crate::error::Result;
use crate::maint::{self, PackResult, PurgeReport, ScrubOptions};
use crate::par2::Par2;
use crate::store::Store;
use crate::util::{self, now_secs};

pub const STATUS_FILE: &str = "worker-status.json";
pub const INDEX_FILE: &str = "INDEX.tsv";
const LOCK_FILE: &str = "worker.lock";
const SNAPSHOT_DIR: &str = "snapshots";

#[derive(Clone, Debug)]
pub struct WorkerOptions {
    /// Seal open packs untouched for this long (an interrupted upload).
    pub seal_idle: Duration,
    pub scrub_every_days: u32,
    /// Stop starting new PAR2 or scrub work after this long.
    pub budget: Duration,
    /// Daily catalog snapshots to keep.
    pub snapshots_kept: usize,
    /// Run the daily steps even if they already ran today.
    pub force_daily: bool,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        WorkerOptions {
            seal_idle: Duration::from_secs(6 * 3600),
            scrub_every_days: 90,
            budget: Duration::from_secs(50 * 60),
            snapshots_kept: 14,
            force_daily: false,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub started: i64,
    pub finished: i64,
    pub par2: Option<String>,
    pub sealed: usize,
    pub protected: Vec<PackResult>,
    pub scrubbed: Vec<PackResult>,
    pub purge: Option<PurgeReport>,
    pub snapshot: Option<String>,
    #[serde(default)]
    pub index: Option<String>,
    pub packs_total: usize,
    pub packs_protected: usize,
    /// Sealed or open packs still waiting for recovery data.
    pub packs_pending: usize,
    pub packs_damaged: Vec<String>,
    pub errors: Vec<String>,
}

/// Run one maintenance cycle. Returns `None` if another run holds the lock.
pub fn run(root: &Path, opts: &WorkerOptions) -> Result<Option<WorkerStatus>> {
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(root.join(LOCK_FILE))?;
    if !util::try_lock(&lock)? {
        return Ok(None);
    }
    let started = Instant::now();
    let deadline = Some(started + opts.budget);
    let mut st = WorkerStatus { started: now_secs(), ..Default::default() };
    let mut store = Store::open(root)?;

    match store.seal_all_open(Some(opts.seal_idle.as_secs() as i64)) {
        Ok(n) => st.sealed = n,
        Err(e) => st.errors.push(format!("sealing idle packs: {e}")),
    }

    match Par2::discover(store.config().par2_path.as_deref(), root) {
        Ok(par2) => {
            st.par2 = Some(par2.path().display().to_string());
            match maint::protect(&store, &par2, deadline, &mut |_| {}) {
                Ok(r) => st.protected = r,
                Err(e) => st.errors.push(format!("protecting packs: {e}")),
            }
            let scrub = ScrubOptions {
                due_before: Some(now_secs() - opts.scrub_every_days as i64 * 86_400 + i64::from(opts.scrub_every_days == 0)),
                deadline,
            };
            match maint::scrub(&store, &par2, scrub, &mut |_| {}) {
                Ok(r) => st.scrubbed = r,
                Err(e) => st.errors.push(format!("checking packs: {e}")),
            }
        }
        Err(e) => st.errors.push(format!("par2 not available, so no recovery data was made: {e}")),
    }

    if opts.force_daily || daily_due(&store, "last_purge")? {
        match maint::purge(&mut store, None) {
            Ok(r) => {
                st.purge = Some(r);
                store.catalog().meta_set("last_purge", &now_secs().to_string())?;
            }
            Err(e) => st.errors.push(format!("purging trash: {e}")),
        }
    }
    if opts.force_daily || daily_due(&store, "last_snapshot")? {
        match snapshot(&store, opts.snapshots_kept) {
            Ok(p) => {
                st.snapshot = Some(p.display().to_string());
                store.catalog().meta_set("last_snapshot", &now_secs().to_string())?;
            }
            Err(e) => st.errors.push(format!("catalog snapshot: {e}")),
        }
        match write_index(&store) {
            Ok(p) => st.index = Some(p.display().to_string()),
            Err(e) => st.errors.push(format!("writing {INDEX_FILE}: {e}")),
        }
    }

    let packs = cat::packs(store.conn())?;
    st.packs_total = packs.len();
    st.packs_protected = packs.iter().filter(|p| p.state == PackState::Protected).count();
    st.packs_pending = packs.iter().filter(|p| matches!(p.state, PackState::Open | PackState::Sealed)).count();
    st.packs_damaged = packs.iter().filter(|p| p.state == PackState::Damaged).map(|p| p.name.clone()).collect();
    st.finished = now_secs();
    util::atomic_write(&root.join(STATUS_FILE), &serde_json::to_vec_pretty(&st)?)?;
    Ok(Some(st))
}

pub fn read_status(root: &Path) -> Option<WorkerStatus> {
    serde_json::from_slice(&fs::read(root.join(STATUS_FILE)).ok()?).ok()
}

fn daily_due(store: &Store, key: &str) -> Result<bool> {
    let last: i64 = store.catalog().meta_get(key)?.and_then(|v| v.parse().ok()).unwrap_or(0);
    Ok(now_secs() - last >= 20 * 3600)
}

/// Copy the catalog to `snapshots/catalog-YYYY-MM-DD.db` with PAR2, keeping
/// the newest `keep` snapshots.
pub fn snapshot(store: &Store, keep: usize) -> Result<PathBuf> {
    let dir = store.root().join(SNAPSHOT_DIR);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("catalog-{}.db", util::date(now_secs())));
    if path.exists() {
        fs::remove_file(&path)?;
        crate::par2::remove_recovery_files(&path)?;
    }
    store.conn().execute("VACUUM INTO ?1", [path.to_string_lossy()])?;
    fs::File::open(&path)?.sync_all()?;
    if let Ok(par2) = Par2::discover(store.config().par2_path.as_deref(), store.root()) {
        par2.create(&path, 10)?;
    }
    let mut snaps: Vec<PathBuf> =
        fs::read_dir(&dir)?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "db")).collect();
    snaps.sort();
    while snaps.len() > keep {
        let old = snaps.remove(0);
        crate::par2::remove_recovery_files(&old)?;
        fs::remove_file(&old)?;
    }
    Ok(path)
}

/// Tabs, newlines, and backslashes in names, escaped so each file is one line.
fn tsv_field(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains(['\t', '\n', '\r', '\\']) {
        s.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n").replace('\r', "\\r").into()
    } else {
        s.into()
    }
}

/// Write `INDEX.tsv` at the archive root: one line per file outside the
/// trash, with where its bytes are, so any file can be found with `grep` and
/// extracted with standard tools (see RECOVERY.txt in any pack):
///
/// ```text
/// tail -c +$((START + 1)) packs/PACK | head -c LENGTH | zstd -dc | tail -c +$((OFFSET + 1)) | head -c SIZE > FILE
/// ```
///
/// Protected with PAR2 when par2 is available.
pub fn write_index(store: &Store) -> Result<PathBuf> {
    let path = store.root().join(INDEX_FILE);
    let tmp = store.root().join(format!("{INDEX_FILE}.tmp"));
    let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
    writeln!(
        w,
        "# Every file in this archive, updated daily. To extract one with standard tools:\n\
         #   tail -c +$((START + 1)) packs/PACK | head -c LENGTH | zstd -dc | tail -c +$((OFFSET + 1)) | head -c SIZE > FILE\n\
         # START and LENGTH locate the compressed data in the pack, OFFSET the file within it once decompressed.\n\
         # A SIZE of 0 is an empty file.\n\
         # MEMBER is the tar member holding it and STORED_AS the file's name there (or inside a small-file block).\n\
         # Names with a tab, newline, or backslash show them as \\t, \\n, and \\\\. See RECOVERY.txt in any pack.\n\
         path\tsize\tsha256\tpack\tstart\tlength\toffset\tmember\tstored_as"
    )?;
    let c = store.conn();
    let mut st = c.prepare(
        "WITH RECURSIVE tree(id, path) AS ( \
           SELECT id, name FROM nodes WHERE parent = ?1 \
           UNION ALL SELECT n.id, tree.path || '/' || n.name FROM nodes n JOIN tree ON n.parent = tree.id) \
         SELECT tree.path, n.size, c.sha256, p.name, b.data_off, b.data_len, c.blob_off, b.member, \
           (SELECT e.path FROM pack_entries e WHERE e.blob_id = c.blob_id AND e.sha256 = c.sha256 ORDER BY e.id LIMIT 1) \
         FROM tree JOIN nodes n ON n.id = tree.id JOIN contents c ON c.id = n.content_id \
         JOIN blobs b ON b.id = c.blob_id JOIN packs p ON p.id = b.pack_id \
         WHERE n.kind = 'f' ORDER BY tree.path",
    )?;
    let mut rows = st.query([cat::ROOT_ID])?;
    while let Some(r) = rows.next()? {
        let (path, size, sha, pack): (String, i64, Vec<u8>, String) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
        let (start, length, offset): (i64, i64, i64) = (r.get(4)?, r.get(5)?, r.get(6)?);
        let (member, stored): (String, Option<String>) = (r.get(7)?, r.get(8)?);
        let sha = crate::hash::Digest::from_slice(&sha).map(|d| d.to_hex()).unwrap_or_default();
        writeln!(
            w,
            "/{}\t{size}\t{sha}\t{}\t{start}\t{length}\t{offset}\t{}\t{}",
            tsv_field(&path),
            tsv_field(&pack),
            tsv_field(&member),
            tsv_field(stored.as_deref().unwrap_or(""))
        )?;
    }
    let f = w.into_inner().map_err(|e| e.into_error())?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, &path)?;
    util::sync_dir(store.root())?;
    crate::par2::remove_recovery_files(&path)?;
    if let Ok(par2) = Par2::discover(store.config().par2_path.as_deref(), store.root()) {
        par2.create(&path, 10)?;
    }
    Ok(path)
}
