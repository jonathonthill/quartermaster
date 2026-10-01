//! The catalog: an SQLite database holding the virtual file tree users browse,
//! and where each file's bytes live inside packs.
//!
//! Folder rows carry recursive `size`, `stored`, and `files` totals so
//! listings are instant. Every change that adds, removes, or moves a node
//! adjusts the totals of its ancestors. `size` is the original size; `stored`
//! is the space used in the archive (compressed, with each distinct content
//! counted once, on the first file that has it; duplicates count zero).
//!
//! Data lives in projects: a folder sent to the archive becomes a project,
//! and every node under it carries the project's id. Projects are frozen
//! (see [`crate::store`]) and self-contained: deduplication happens only
//! within a project, and a pack holds data from one project only. Folders
//! outside any project just organize projects.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::hash::Digest;
use crate::vpath::VPath;

pub const ROOT_ID: i64 = 1;
pub const TRASH_ID: i64 = 2;
pub const SCHEMA_VERSION: i64 = 4;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS packs(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL UNIQUE,
  job TEXT,
  state TEXT NOT NULL,
  bytes INTEGER NOT NULL DEFAULT 0,
  created INTEGER NOT NULL,
  sealed_at INTEGER,
  protected_at INTEGER,
  checked_at INTEGER,
  check_result TEXT,
  project TEXT
);
CREATE INDEX IF NOT EXISTS packs_job ON packs(job, state);
CREATE TABLE IF NOT EXISTS blobs(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  pack_id INTEGER NOT NULL REFERENCES packs(id),
  member TEXT NOT NULL,
  kind TEXT NOT NULL,
  data_off INTEGER NOT NULL,
  data_len INTEGER NOT NULL,
  raw_len INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS blobs_pack ON blobs(pack_id);
CREATE TABLE IF NOT EXISTS contents(
  id INTEGER PRIMARY KEY,
  project TEXT NOT NULL DEFAULT '',
  sha256 BLOB NOT NULL,
  size INTEGER NOT NULL,
  blob_id INTEGER NOT NULL REFERENCES blobs(id),
  blob_off INTEGER NOT NULL,
  refs INTEGER NOT NULL DEFAULT 0,
  UNIQUE(project, sha256)
);
CREATE INDEX IF NOT EXISTS contents_size ON contents(project, size);
CREATE INDEX IF NOT EXISTS contents_blob ON contents(blob_id);
CREATE TABLE IF NOT EXISTS nodes(
  id INTEGER PRIMARY KEY,
  parent INTEGER REFERENCES nodes(id),
  name TEXT NOT NULL,
  kind TEXT NOT NULL,
  content_id INTEGER REFERENCES contents(id),
  target TEXT,
  size INTEGER NOT NULL DEFAULT 0,
  files INTEGER NOT NULL DEFAULT 0,
  mtime_ns INTEGER NOT NULL DEFAULT 0,
  mode INTEGER NOT NULL DEFAULT 0,
  created INTEGER NOT NULL,
  trashed_at INTEGER,
  trash_origin TEXT,
  project TEXT,
  stored INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS nodes_child ON nodes(parent, name);
CREATE TABLE IF NOT EXISTS projects(
  id TEXT PRIMARY KEY,
  root INTEGER NOT NULL UNIQUE REFERENCES nodes(id),
  created INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS nodes_content ON nodes(content_id);
CREATE INDEX IF NOT EXISTS nodes_name ON nodes(name COLLATE NOCASE);
CREATE TABLE IF NOT EXISTS pack_entries(
  id INTEGER PRIMARY KEY,
  pack_id INTEGER NOT NULL REFERENCES packs(id),
  path TEXT NOT NULL,
  kind TEXT NOT NULL,
  sha256 BLOB,
  size INTEGER NOT NULL DEFAULT 0,
  mtime_ns INTEGER NOT NULL DEFAULT 0,
  mode INTEGER NOT NULL DEFAULT 0,
  blob_id INTEGER REFERENCES blobs(id),
  blob_off INTEGER,
  target TEXT,
  same_as TEXT
);
CREATE INDEX IF NOT EXISTS pack_entries_pack ON pack_entries(pack_id);
CREATE INDEX IF NOT EXISTS pack_entries_sha ON pack_entries(sha256);
CREATE VIRTUAL TABLE IF NOT EXISTS name_index USING fts5(words, folders, tokenize = "unicode61 remove_diacritics 2", prefix = '2 3 4');
INSERT OR IGNORE INTO nodes(id, parent, name, kind, created) VALUES (1, NULL, '', 'd', 0);
INSERT OR IGNORE INTO nodes(id, parent, name, kind, created) VALUES (2, NULL, '.trash', 'd', 0);
"#;

pub fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Dir,
    File,
    Symlink,
}

impl NodeKind {
    pub fn code(self) -> &'static str {
        match self {
            NodeKind::Dir => "d",
            NodeKind::File => "f",
            NodeKind::Symlink => "l",
        }
    }
    pub fn from_code(s: &str) -> Result<Self> {
        match s {
            "d" => Ok(NodeKind::Dir),
            "f" => Ok(NodeKind::File),
            "l" => Ok(NodeKind::Symlink),
            _ => Err(Error::Corrupt(format!("unknown node kind {s:?}"))),
        }
    }
}

/// One node in the virtual tree, as shown to users.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub id: i64,
    pub name: String,
    pub kind: NodeKind,
    /// Original file size, or the recursive total for folders.
    pub size: u64,
    /// Space used in the archive: compressed, and zero for a duplicate of a
    /// file stored earlier in the project. The recursive total for folders.
    #[serde(default)]
    pub stored: u64,
    /// 1 for files; the recursive file count for folders.
    pub files: u64,
    pub mtime_ns: i64,
    pub mode: u32,
    /// When it was archived (seconds since epoch).
    pub created: i64,
    pub sha256: Option<Digest>,
    pub target: Option<String>,
    pub trashed_at: Option<i64>,
    pub trash_origin: Option<String>,
    /// This is a project's top folder.
    #[serde(default)]
    pub is_project: bool,
    /// This is inside a project (below its top folder), so it can't be changed.
    #[serde(default)]
    pub in_project: bool,
    /// From `stat` only: the path of the project's top folder, for items in a project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    #[serde(skip)]
    pub parent: Option<i64>,
    #[serde(skip)]
    pub content_id: Option<i64>,
    #[serde(skip)]
    pub project: Option<String>,
}

const ENTRY_COLS: &str = "n.id, n.name, n.kind, n.size, n.files, n.mtime_ns, n.mode, n.created, c.sha256, \
     n.target, n.trashed_at, n.trash_origin, n.parent, n.content_id, n.project, \
     EXISTS(SELECT 1 FROM projects pr WHERE pr.root = n.id), n.stored";

fn entry_from_row(r: &Row) -> rusqlite::Result<Entry> {
    let kind: String = r.get(2)?;
    let sha: Option<Vec<u8>> = r.get(8)?;
    let project: Option<String> = r.get(14)?;
    let is_project: bool = r.get(15)?;
    Ok(Entry {
        stored: r.get::<_, i64>(16)?.max(0) as u64,
        in_project: project.is_some() && !is_project,
        is_project,
        project,
        project_path: None,
        id: r.get(0)?,
        name: r.get(1)?,
        kind: NodeKind::from_code(&kind).unwrap_or(NodeKind::File),
        size: r.get::<_, i64>(3)? as u64,
        files: r.get::<_, i64>(4)? as u64,
        mtime_ns: r.get(5)?,
        mode: r.get::<_, i64>(6)? as u32,
        created: r.get(7)?,
        sha256: sha.and_then(|b| Digest::from_slice(&b)),
        target: r.get(9)?,
        trashed_at: r.get(10)?,
        trash_origin: r.get(11)?,
        parent: r.get(12)?,
        content_id: r.get(13)?,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackState {
    Open,
    Sealed,
    Protected,
    Damaged,
}

impl PackState {
    pub fn as_str(self) -> &'static str {
        match self {
            PackState::Open => "open",
            PackState::Sealed => "sealed",
            PackState::Protected => "protected",
            PackState::Damaged => "damaged",
        }
    }
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "open" => PackState::Open,
            "sealed" => PackState::Sealed,
            "protected" => PackState::Protected,
            "damaged" => PackState::Damaged,
            _ => return Err(Error::Corrupt(format!("unknown pack state {s:?}"))),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pack {
    pub id: i64,
    pub name: String,
    pub job: Option<String>,
    pub state: PackState,
    pub bytes: u64,
    pub created: i64,
    pub sealed_at: Option<i64>,
    pub protected_at: Option<i64>,
    pub checked_at: Option<i64>,
    pub check_result: Option<String>,
    /// The project whose data the pack holds (`None` for packs from before projects).
    #[serde(default)]
    pub project: Option<String>,
}

fn pack_from_row(r: &Row) -> rusqlite::Result<Pack> {
    let state: String = r.get(3)?;
    Ok(Pack {
        id: r.get(0)?,
        name: r.get(1)?,
        job: r.get(2)?,
        state: PackState::parse(&state).unwrap_or(PackState::Damaged),
        bytes: r.get::<_, i64>(4)? as u64,
        created: r.get(5)?,
        sealed_at: r.get(6)?,
        protected_at: r.get(7)?,
        checked_at: r.get(8)?,
        check_result: r.get(9)?,
        project: r.get(10)?,
    })
}

const PACK_COLS: &str = "id, name, job, state, bytes, created, sealed_at, protected_at, checked_at, check_result, project";

/// Where a file's bytes live: a range of the decompressed stream of a blob.
#[derive(Clone, Debug)]
pub struct Location {
    pub content_id: i64,
    pub size: u64,
    pub sha256: Digest,
    pub pack_id: i64,
    pub pack_name: String,
    pub data_off: u64,
    pub data_len: u64,
    pub blob_off: u64,
}

pub struct Catalog {
    conn: Connection,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Catalog> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        let v: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0))
            .optional()
            .or_else(|e| if e.to_string().contains("no such table") { Ok(None) } else { Err(e) })?;
        let v = v.as_deref().map(|v| v.parse::<i64>().unwrap_or(i64::MAX));
        if let Some(v) = v.filter(|&v| v > SCHEMA_VERSION) {
            return Err(Error::other(format!("catalog schema {v} is newer than this program supports")));
        }
        if v.is_some_and(|v| v < 3) {
            migrate_to_3(&conn)?;
        }
        if v.is_some_and(|v| v < 4) {
            conn.execute_batch("ALTER TABLE nodes ADD COLUMN stored INTEGER NOT NULL DEFAULT 0;")?;
        }
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        let mut cat = Catalog { conn };
        match v {
            None => cat.meta_set("schema_version", &SCHEMA_VERSION.to_string())?,
            Some(SCHEMA_VERSION) => {}
            Some(old) => {
                let tx = cat.conn.transaction()?;
                if old == 1 {
                    // Version 2 added the word index; fill it from the existing tree.
                    reindex_all(&tx)?;
                }
                // Version 4 added space used per file and folder.
                recount_stored(&tx)?;
                tx.execute("UPDATE meta SET value=?1 WHERE key='schema_version'", [SCHEMA_VERSION.to_string()])?;
                tx.commit()?;
            }
        }
        Ok(cat)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute("INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [key, value])?;
        Ok(())
    }
}

/// Version 3 adds projects. Data stored before then belongs to no project:
/// it stays readable and can be reorganized, and its contents deduplicate
/// only with each other (project `''`).
fn migrate_to_3(c: &Connection) -> Result<()> {
    // Rebuilding `contents` (for its new unique key) needs foreign keys off.
    c.pragma_update(None, "foreign_keys", "OFF")?;
    c.execute_batch(
        "BEGIN;
         ALTER TABLE nodes ADD COLUMN project TEXT;
         ALTER TABLE packs ADD COLUMN project TEXT;
         CREATE TABLE contents_v3(
           id INTEGER PRIMARY KEY,
           project TEXT NOT NULL DEFAULT '',
           sha256 BLOB NOT NULL,
           size INTEGER NOT NULL,
           blob_id INTEGER NOT NULL REFERENCES blobs(id),
           blob_off INTEGER NOT NULL,
           refs INTEGER NOT NULL DEFAULT 0,
           UNIQUE(project, sha256)
         );
         INSERT INTO contents_v3(id, project, sha256, size, blob_id, blob_off, refs)
           SELECT id, '', sha256, size, blob_id, blob_off, refs FROM contents;
         DROP TABLE contents;
         ALTER TABLE contents_v3 RENAME TO contents;
         COMMIT;",
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tree queries. These take `&Connection` so they compose inside transactions.
// ---------------------------------------------------------------------------

pub fn entry(c: &Connection, id: i64) -> Result<Entry> {
    c.query_row(
        &format!("SELECT {ENTRY_COLS} FROM nodes n LEFT JOIN contents c ON c.id = n.content_id WHERE n.id = ?1"),
        [id],
        entry_from_row,
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("node {id}")))
}

pub fn child(c: &Connection, parent: i64, name: &str) -> Result<Option<Entry>> {
    Ok(c.query_row(
        &format!("SELECT {ENTRY_COLS} FROM nodes n LEFT JOIN contents c ON c.id = n.content_id WHERE n.parent = ?1 AND n.name = ?2"),
        params![parent, name],
        entry_from_row,
    )
    .optional()?)
}

pub fn children(c: &Connection, parent: i64) -> Result<Vec<Entry>> {
    let mut st = c.prepare_cached(&format!(
        "SELECT {ENTRY_COLS} FROM nodes n LEFT JOIN contents c ON c.id = n.content_id WHERE n.parent = ?1 \
         ORDER BY n.kind <> 'd', n.name COLLATE NOCASE"
    ))?;
    let rows = st.query_map([parent], entry_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Resolve a path to its node, or `None` if any component is missing.
pub fn resolve(c: &Connection, path: &VPath) -> Result<Option<Entry>> {
    let mut cur = entry(c, ROOT_ID)?;
    for name in path.components() {
        if cur.kind != NodeKind::Dir {
            return Ok(None);
        }
        match child(c, cur.id, name)? {
            Some(e) => cur = e,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

pub fn resolve_dir(c: &Connection, path: &VPath) -> Result<Entry> {
    match resolve(c, path)? {
        Some(e) if e.kind == NodeKind::Dir => Ok(e),
        Some(_) => Err(Error::NotADirectory(path.to_string())),
        None => Err(Error::NotFound(path.to_string())),
    }
}

/// Full path of a node. Nodes inside the trash resolve relative to the trash.
pub fn path_of(c: &Connection, id: i64) -> Result<(VPath, bool)> {
    let mut names = Vec::new();
    let mut cur = id;
    loop {
        if cur == ROOT_ID {
            break;
        }
        if cur == TRASH_ID {
            names.reverse();
            return Ok((VPath::parse(&names.join("/")).unwrap_or_default(), true));
        }
        let (parent, name): (Option<i64>, String) =
            c.query_row("SELECT parent, name FROM nodes WHERE id=?1", [cur], |r| Ok((r.get(0)?, r.get(1)?)))?;
        names.push(name);
        cur = parent.ok_or_else(|| Error::Corrupt(format!("node {id} is detached")))?;
    }
    names.reverse();
    let mut p = VPath::root();
    for n in names {
        p = p.join(&n)?;
    }
    Ok((p, false))
}

/// Add `dsize`/`dfiles`/`dstored` to a folder and all its ancestors.
fn bump(c: &Connection, dir: i64, dsize: i64, dfiles: i64, dstored: i64) -> Result<()> {
    if dsize == 0 && dfiles == 0 && dstored == 0 {
        return Ok(());
    }
    let mut cur = Some(dir);
    while let Some(id) = cur {
        c.execute(
            "UPDATE nodes SET size = size + ?1, files = files + ?2, stored = stored + ?3 WHERE id = ?4",
            params![dsize, dfiles, dstored, id],
        )?;
        cur = c.query_row("SELECT parent FROM nodes WHERE id=?1", [id], |r| r.get(0))?;
    }
    Ok(())
}

/// Compressed bytes of one content: its whole member, or for a file in a
/// block of small files, its share of the block by size.
const CONTENT_STORED: &str = "SELECT CASE WHEN b.kind = 's' AND b.raw_len > 0 THEN b.data_len * c.size / b.raw_len ELSE b.data_len END \
     FROM contents c JOIN blobs b ON b.id = c.blob_id WHERE c.id = ?1";

/// Recompute every node's `stored` from scratch (upgrading older catalogs).
pub fn recount_stored(c: &Connection) -> Result<()> {
    c.execute("UPDATE nodes SET stored = 0", [])?;
    // The first file (lowest id) with each content carries its bytes.
    c.execute(
        "UPDATE nodes SET stored = (SELECT CASE WHEN b.kind = 's' AND b.raw_len > 0 THEN b.data_len * ct.size / b.raw_len ELSE b.data_len END \
           FROM contents ct JOIN blobs b ON b.id = ct.blob_id WHERE ct.id = nodes.content_id) \
         WHERE kind = 'f' AND content_id IS NOT NULL AND id = (SELECT MIN(n2.id) FROM nodes n2 WHERE n2.content_id = nodes.content_id)",
        [],
    )?;
    // Folder totals, deepest first.
    let mut rows: Vec<(i64, Option<i64>, String, i64)> = c
        .prepare("SELECT id, parent, kind, stored FROM nodes")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let parent: std::collections::HashMap<i64, Option<i64>> = rows.iter().map(|r| (r.0, r.1)).collect();
    let mut totals: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    rows.retain(|r| r.2 == "f");
    for (id, _, _, stored) in rows {
        let mut cur = parent.get(&id).copied().flatten();
        while let Some(p) = cur {
            *totals.entry(p).or_default() += stored;
            cur = parent.get(&p).copied().flatten();
        }
    }
    let mut st = c.prepare("UPDATE nodes SET stored = ?1 WHERE id = ?2")?;
    for (id, total) in totals {
        st.execute(params![total, id])?;
    }
    Ok(())
}

pub fn mkdir(c: &Connection, parent: i64, name: &str, mtime_ns: i64, mode: u32) -> Result<i64> {
    crate::vpath::validate_name(name)?;
    if parent == ROOT_ID && name == crate::vpath::RESERVED_ROOT_NAME {
        return Err(Error::InvalidPath(format!("\"{name}\" is reserved")));
    }
    if let Some(existing) = child(c, parent, name)? {
        return match existing.kind {
            NodeKind::Dir => Ok(existing.id),
            _ => Err(Error::AlreadyExists(name.to_string())),
        };
    }
    c.execute(
        "INSERT INTO nodes(parent, name, kind, mtime_ns, mode, created, project) \
         VALUES(?1, ?2, 'd', ?3, ?4, ?5, (SELECT project FROM nodes WHERE id = ?1))",
        params![parent, name, mtime_ns, mode as i64, now_secs()],
    )?;
    let id = c.last_insert_rowid();
    index_new(c, id, parent, name)?;
    Ok(id)
}

/// Create a project's top folder in `parent`, which must not be inside a
/// project. An existing project there is returned as is. Returns (node, project id).
pub fn create_project(c: &Connection, parent: i64, name: &str, mtime_ns: i64, mode: u32) -> Result<(i64, String)> {
    let pe = entry(c, parent)?;
    if pe.project.is_some() {
        return Err(Error::InvalidPath(format!("{} is inside a project; projects can't contain other projects", path_of(c, parent)?.0)));
    }
    if let Some(existing) = child(c, parent, name)? {
        return match (existing.is_project, existing.project) {
            (true, Some(p)) => Ok((existing.id, p)),
            _ => Err(Error::AlreadyExists(format!("{} is already here and isn't a project", existing.name))),
        };
    }
    let id = mkdir(c, parent, name, mtime_ns, mode)?;
    let project = crate::util::random_hex(16);
    c.execute("UPDATE nodes SET project=?1 WHERE id=?2", params![project, id])?;
    c.execute("INSERT INTO projects(id, root, created) VALUES(?1, ?2, ?3)", params![project, id, now_secs()])?;
    Ok((id, project))
}

/// Make an existing, empty folder a project with a known id (rebuilding a catalog).
pub fn adopt_project(c: &Connection, node: i64, project: &str) -> Result<()> {
    c.execute("UPDATE nodes SET project=?1 WHERE id=?2", params![project, node])?;
    c.execute("INSERT INTO projects(id, root, created) VALUES(?1, ?2, ?3)", params![project, node, now_secs()])?;
    Ok(())
}

/// The top folder of a project, if the project exists.
pub fn project_root(c: &Connection, project: &str) -> Result<Option<i64>> {
    Ok(c.query_row("SELECT root FROM projects WHERE id=?1", [project], |r| r.get(0)).optional()?)
}

/// The project a new item at `path` would belong to: that of the deepest
/// part of the path that exists. `None` means outside every project.
pub fn project_for(c: &Connection, path: &VPath) -> Result<Option<String>> {
    let mut cur = entry(c, ROOT_ID)?;
    for name in path.components() {
        match child(c, cur.id, name)? {
            Some(e) => cur = e,
            None => break,
        }
    }
    Ok(cur.project)
}

/// Every project whose top folder is `id` or below it.
pub fn projects_under(c: &Connection, id: i64) -> Result<Vec<String>> {
    let mut st = c.prepare(
        "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION ALL SELECT n.id FROM nodes n JOIN sub ON n.parent = sub.id AND n.kind = 'd') \
         SELECT p.id FROM projects p JOIN sub ON sub.id = p.root",
    )?;
    Ok(st.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
}

/// Create a folder and any missing parents. Returns the folder's id.
pub fn mkdir_p(c: &Connection, path: &VPath) -> Result<i64> {
    let mut cur = ROOT_ID;
    for name in path.components() {
        cur = mkdir(c, cur, name, 0, 0o755)?;
    }
    Ok(cur)
}

pub struct NewFile<'a> {
    pub parent: i64,
    pub name: &'a str,
    pub content_id: i64,
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: u32,
}

/// Insert a file node referencing existing content. Fails if the name is taken.
pub fn insert_file(c: &Connection, f: &NewFile) -> Result<i64> {
    crate::vpath::validate_name(f.name)?;
    if child(c, f.parent, f.name)?.is_some() {
        return Err(Error::AlreadyExists(f.name.to_string()));
    }
    c.execute(
        "INSERT INTO nodes(parent, name, kind, content_id, size, files, mtime_ns, mode, created, project) \
         VALUES(?1, ?2, 'f', ?3, ?4, 1, ?5, ?6, ?7, (SELECT project FROM nodes WHERE id = ?1))",
        params![f.parent, f.name, f.content_id, f.size as i64, f.mtime_ns, f.mode as i64, now_secs()],
    )?;
    let id = c.last_insert_rowid();
    // The first file with this content carries its bytes; later ones are duplicates.
    let refs: i64 = c.query_row("SELECT refs FROM contents WHERE id = ?1", [f.content_id], |r| r.get(0))?;
    let stored: i64 = if refs == 0 { c.query_row(CONTENT_STORED, [f.content_id], |r| r.get(0))? } else { 0 };
    c.execute("UPDATE nodes SET stored = ?1 WHERE id = ?2", params![stored, id])?;
    c.execute("UPDATE contents SET refs = refs + 1 WHERE id = ?1", [f.content_id])?;
    bump(c, f.parent, f.size as i64, 1, stored)?;
    index_new(c, id, f.parent, f.name)?;
    Ok(id)
}

pub fn insert_symlink(c: &Connection, parent: i64, name: &str, target: &str, mtime_ns: i64) -> Result<i64> {
    crate::vpath::validate_name(name)?;
    if child(c, parent, name)?.is_some() {
        return Err(Error::AlreadyExists(name.to_string()));
    }
    c.execute(
        "INSERT INTO nodes(parent, name, kind, target, mtime_ns, mode, created, project) \
         VALUES(?1, ?2, 'l', ?3, ?4, 511, ?5, (SELECT project FROM nodes WHERE id = ?1))",
        params![parent, name, target, mtime_ns, now_secs()],
    )?;
    let id = c.last_insert_rowid();
    index_new(c, id, parent, name)?;
    Ok(id)
}

/// Move or rename a node. Refuses to overwrite and to move a folder into itself.
pub fn move_node(c: &Connection, id: i64, new_parent: i64, new_name: &str) -> Result<()> {
    crate::vpath::validate_name(new_name)?;
    if id == ROOT_ID || id == TRASH_ID {
        return Err(Error::InvalidPath("cannot move the archive root".into()));
    }
    if new_parent == ROOT_ID && new_name == crate::vpath::RESERVED_ROOT_NAME {
        return Err(Error::InvalidPath(format!("\"{new_name}\" is reserved")));
    }
    let e = entry(c, id)?;
    // Refuse cycles: new_parent must not be id or one of its descendants.
    let mut cur = Some(new_parent);
    while let Some(p) = cur {
        if p == id {
            return Err(Error::InvalidPath("cannot move a folder into itself".into()));
        }
        cur = c.query_row("SELECT parent FROM nodes WHERE id=?1", [p], |r| r.get(0))?;
    }
    if let Some(existing) = child(c, new_parent, new_name)? {
        if existing.id != id {
            return Err(Error::AlreadyExists(new_name.to_string()));
        }
    }
    let old_parent = e.parent.ok_or_else(|| Error::Corrupt("node without parent".into()))?;
    c.execute("UPDATE nodes SET parent=?1, name=?2 WHERE id=?3", params![new_parent, new_name, id])?;
    if old_parent != new_parent {
        bump(c, old_parent, -(e.size as i64), -(e.files as i64), -(e.stored as i64))?;
        bump(c, new_parent, e.size as i64, e.files as i64, e.stored as i64)?;
    }
    // Names or folders changed for the whole subtree (or it left for the trash).
    reindex_subtree(c, id)?;
    Ok(())
}

/// Move a node to the trash, remembering where it came from.
pub fn trash(c: &Connection, id: i64) -> Result<()> {
    let (origin, in_trash) = path_of(c, id)?;
    if in_trash {
        return Err(Error::InvalidPath("already in the trash".into()));
    }
    move_node(c, id, TRASH_ID, &id.to_string())?;
    c.execute("UPDATE nodes SET trashed_at=?1, trash_origin=?2 WHERE id=?3", params![now_secs(), origin.to_string(), id])?;
    Ok(())
}

/// Put a trashed node back where it came from (or at `to` if given).
pub fn restore(c: &Connection, id: i64, to: Option<&VPath>) -> Result<VPath> {
    let e = entry(c, id)?;
    if e.parent != Some(TRASH_ID) {
        return Err(Error::InvalidPath("not in the trash".into()));
    }
    let dest = match to {
        Some(p) => p.clone(),
        None => VPath::parse(e.trash_origin.as_deref().unwrap_or(""))?,
    };
    let name = dest.name().ok_or_else(|| Error::InvalidPath("cannot restore to the root".into()))?;
    let parent = mkdir_p(c, &dest.parent().unwrap_or_default())?;
    move_node(c, id, parent, name)?;
    c.execute("UPDATE nodes SET trashed_at=NULL, trash_origin=NULL WHERE id=?1", [id])?;
    Ok(dest)
}

/// Permanently remove a node and its subtree from the tree, releasing content refs.
pub fn delete_subtree(c: &Connection, id: i64) -> Result<()> {
    let e = entry(c, id)?;
    for ch in children(c, id)? {
        delete_subtree(c, ch.id)?;
    }
    if let Some(parent) = e.parent {
        if e.kind == NodeKind::File {
            bump(c, parent, -(e.size as i64), -1, -(e.stored as i64))?;
        }
    }
    if let Some(cid) = e.content_id {
        c.execute("UPDATE contents SET refs = refs - 1 WHERE id = ?1", [cid])?;
        // Its bytes are still stored for the other files with this content.
        if e.stored > 0 {
            let heir: Option<(i64, i64)> = c
                .query_row("SELECT id, parent FROM nodes WHERE content_id = ?1 AND id <> ?2 ORDER BY id LIMIT 1", params![cid, id], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()?;
            if let Some((heir, heir_parent)) = heir {
                c.execute("UPDATE nodes SET stored = ?1 WHERE id = ?2", params![e.stored as i64, heir])?;
                bump(c, heir_parent, 0, 0, e.stored as i64)?;
            }
        }
    }
    c.execute("DELETE FROM name_index WHERE rowid=?1", [id])?;
    c.execute("DELETE FROM projects WHERE root=?1", [id])?;
    c.execute("DELETE FROM nodes WHERE id=?1", [id])?;
    Ok(())
}

/// Delete trash entries trashed before `cutoff`. Returns how many were purged.
pub fn purge_trash(c: &Connection, cutoff: i64) -> Result<usize> {
    let ids: Vec<i64> = {
        let mut st = c.prepare("SELECT id FROM nodes WHERE parent=?1 AND trashed_at < ?2")?;
        st.query_map(params![TRASH_ID, cutoff], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    for id in &ids {
        delete_subtree(c, *id)?;
    }
    Ok(ids.len())
}

// ---------------------------------------------------------------------------
// Word index. Each node outside the trash has a row whose `words` are its
// name (expanded; see `words::expand`) and whose `folders` are the names of
// the folders above it, so "dairy fastq" finds fastq files under a dairy folder.
// ---------------------------------------------------------------------------

/// Index text for the folders above `dir` (and `dir` itself), or `None` if it's in the trash.
fn folders_text(c: &Connection, dir: i64) -> Result<Option<String>> {
    let mut names = Vec::new();
    let mut cur = dir;
    loop {
        if cur == ROOT_ID {
            break;
        }
        if cur == TRASH_ID {
            return Ok(None);
        }
        let (parent, name): (Option<i64>, String) =
            c.prepare_cached("SELECT parent, name FROM nodes WHERE id=?1")?.query_row([cur], |r| Ok((r.get(0)?, r.get(1)?)))?;
        names.push(crate::words::expand(&name));
        match parent {
            Some(p) => cur = p,
            None => return Ok(None),
        }
    }
    names.reverse();
    Ok(Some(names.join(" ")))
}

fn index_new(c: &Connection, id: i64, parent: i64, name: &str) -> Result<()> {
    if let Some(folders) = folders_text(c, parent)? {
        c.prepare_cached("INSERT INTO name_index(rowid, words, folders) VALUES(?1, ?2, ?3)")?.execute(params![
            id,
            crate::words::expand(name),
            folders
        ])?;
    }
    Ok(())
}

/// Re-index a node and everything under it (after a rename or move).
fn reindex_subtree(c: &Connection, id: i64) -> Result<()> {
    let parent: Option<i64> = c.query_row("SELECT parent FROM nodes WHERE id=?1", [id], |r| r.get(0))?;
    let folders = match parent {
        Some(p) => folders_text(c, p)?,
        None => None,
    };
    let mut stack = vec![(id, folders)];
    while let Some((node, folders)) = stack.pop() {
        let (name, kind): (String, String) =
            c.query_row("SELECT name, kind FROM nodes WHERE id=?1", [node], |r| Ok((r.get(0)?, r.get(1)?)))?;
        c.prepare_cached("DELETE FROM name_index WHERE rowid=?1")?.execute([node])?;
        let words = crate::words::expand(&name);
        if let Some(f) = &folders {
            c.prepare_cached("INSERT INTO name_index(rowid, words, folders) VALUES(?1, ?2, ?3)")?.execute(params![node, words, f])?;
        }
        if kind == "d" {
            let below = folders.map(|f| if f.is_empty() { words.clone() } else { format!("{f} {words}") });
            let kids: Vec<i64> = c
                .prepare_cached("SELECT id FROM nodes WHERE parent=?1")?
                .query_map([node], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for k in kids {
                stack.push((k, below.clone()));
            }
        }
    }
    Ok(())
}

/// Rebuild the whole word index.
pub fn reindex_all(c: &Connection) -> Result<()> {
    c.execute("DELETE FROM name_index", [])?;
    let top: Vec<i64> =
        c.prepare("SELECT id FROM nodes WHERE parent=?1")?.query_map([ROOT_ID], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for id in top {
        reindex_subtree(c, id)?;
    }
    Ok(())
}

/// Search names outside the trash. See [`crate::words`] for the query language.
pub fn search(c: &Connection, query: &str, limit: usize) -> Result<Vec<(VPath, Entry)>> {
    use crate::words::Filter;
    let q = crate::words::parse(query);
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    for f in &q.filters {
        match f {
            Filter::Ext(e) => {
                conds.push(format!(
                    "(n.kind = 'f' AND (lower(n.name) LIKE ?{0} ESCAPE '\\' OR lower(n.name) LIKE ?{1} ESCAPE '\\'))",
                    args.len() + 1,
                    args.len() + 2
                ));
                let e = e.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
                args.push(Box::new(format!("%.{e}")));
                args.push(Box::new(format!("%.{e}.%")));
            }
            Filter::IsDir => conds.push("n.kind = 'd'".into()),
            Filter::IsFile => conds.push("n.kind = 'f'".into()),
            Filter::After(t) => {
                conds.push(format!("n.mtime_ns >= ?{}", args.len() + 1));
                args.push(Box::new(t.saturating_mul(1_000_000_000)));
            }
            Filter::Before(t) => {
                conds.push(format!("n.mtime_ns < ?{}", args.len() + 1));
                args.push(Box::new(t.saturating_mul(1_000_000_000)));
            }
            Filter::Size(op, b) => {
                conds.push(format!("n.size {op} ?{}", args.len() + 1));
                args.push(Box::new(*b as i64));
            }
        }
    }
    let sql = match q.fts_match() {
        Some(m) => {
            conds.insert(0, format!("name_index MATCH ?{}", args.len() + 1));
            args.push(Box::new(m));
            format!(
                "SELECT {ENTRY_COLS} FROM name_index s JOIN nodes n ON n.id = s.rowid LEFT JOIN contents c ON c.id = n.content_id \
                 WHERE {} ORDER BY bm25(name_index, 10.0, 1.0), n.kind <> 'd', n.name COLLATE NOCASE LIMIT {limit}",
                conds.join(" AND ")
            )
        }
        None => format!(
            "SELECT {ENTRY_COLS} FROM nodes n LEFT JOIN contents c ON c.id = n.content_id \
             WHERE n.id IN (SELECT rowid FROM name_index) AND {} ORDER BY n.kind <> 'd', n.name COLLATE NOCASE LIMIT {limit}",
            conds.join(" AND ")
        ),
    };
    let mut st = c.prepare(&sql)?;
    let rows: Vec<Entry> =
        st.query_map(rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())), entry_from_row)?.collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for e in rows {
        let (p, in_trash) = path_of(c, e.id)?;
        if !in_trash {
            out.push((p, e));
        }
    }
    Ok(out)
}

/// Every node under `id` (not including `id`), with paths relative to it.
pub fn walk(c: &Connection, id: i64) -> Result<Vec<(String, Entry)>> {
    let mut out = Vec::new();
    let mut stack = vec![(String::new(), id)];
    while let Some((prefix, dir)) = stack.pop() {
        for ch in children(c, dir)? {
            let rel = if prefix.is_empty() { ch.name.clone() } else { format!("{prefix}/{}", ch.name) };
            if ch.kind == NodeKind::Dir {
                stack.push((rel.clone(), ch.id));
            }
            out.push((rel, ch));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Contents, blobs, and packs.
// ---------------------------------------------------------------------------

// Contents are keyed by project: `project` is a project id, or "" for data
// stored before projects existed.

pub fn content_by_hash(c: &Connection, project: &str, sha: &Digest) -> Result<Option<(i64, u64)>> {
    Ok(c
        .query_row("SELECT id, size FROM contents WHERE project=?1 AND sha256=?2", params![project, &sha.0[..]], |r| {
            Ok((r.get(0)?, r.get::<_, i64>(1)? as u64))
        })
        .optional()?)
}

pub fn size_present(c: &Connection, project: &str, size: u64) -> Result<bool> {
    Ok(c
        .query_row("SELECT 1 FROM contents WHERE project=?1 AND size=?2 LIMIT 1", params![project, size as i64], |_| Ok(()))
        .optional()?
        .is_some())
}

pub fn insert_content(c: &Connection, project: &str, sha: &Digest, size: u64, blob_id: i64, blob_off: u64) -> Result<i64> {
    c.execute(
        "INSERT INTO contents(project, sha256, size, blob_id, blob_off, refs) VALUES(?1, ?2, ?3, ?4, ?5, 0)",
        params![project, &sha.0[..], size as i64, blob_id, blob_off as i64],
    )?;
    Ok(c.last_insert_rowid())
}

pub fn insert_blob(c: &Connection, pack_id: i64, member: &str, kind: &str, data_off: u64, data_len: u64, raw_len: u64) -> Result<i64> {
    c.execute(
        "INSERT INTO blobs(pack_id, member, kind, data_off, data_len, raw_len) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![pack_id, member, kind, data_off as i64, data_len as i64, raw_len as i64],
    )?;
    Ok(c.last_insert_rowid())
}

pub fn location(c: &Connection, content_id: i64) -> Result<Location> {
    c.query_row(
        "SELECT c.id, c.size, c.sha256, p.id, p.name, b.data_off, b.data_len, c.blob_off \
         FROM contents c JOIN blobs b ON b.id = c.blob_id JOIN packs p ON p.id = b.pack_id WHERE c.id = ?1",
        [content_id],
        |r| {
            let sha: Vec<u8> = r.get(2)?;
            Ok(Location {
                content_id: r.get(0)?,
                size: r.get::<_, i64>(1)? as u64,
                sha256: Digest::from_slice(&sha).unwrap_or(Digest([0; 32])),
                pack_id: r.get(3)?,
                pack_name: r.get(4)?,
                data_off: r.get::<_, i64>(5)? as u64,
                data_len: r.get::<_, i64>(6)? as u64,
                blob_off: r.get::<_, i64>(7)? as u64,
            })
        },
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("content {content_id}")))
}

pub fn pack(c: &Connection, id: i64) -> Result<Pack> {
    c.query_row(&format!("SELECT {PACK_COLS} FROM packs WHERE id=?1"), [id], pack_from_row)
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("pack {id}")))
}

pub fn packs(c: &Connection) -> Result<Vec<Pack>> {
    let mut st = c.prepare(&format!("SELECT {PACK_COLS} FROM packs ORDER BY id"))?;
    Ok(st.query_map([], pack_from_row)?.collect::<rusqlite::Result<_>>()?)
}

pub fn packs_in_state(c: &Connection, state: PackState) -> Result<Vec<Pack>> {
    let mut st = c.prepare(&format!("SELECT {PACK_COLS} FROM packs WHERE state=?1 ORDER BY id"))?;
    Ok(st.query_map([state.as_str()], pack_from_row)?.collect::<rusqlite::Result<_>>()?)
}

/// The open pack a job is filling for a project.
pub fn open_pack_for(c: &Connection, job: &str, project: &str) -> Result<Option<Pack>> {
    Ok(c
        .query_row(
            &format!("SELECT {PACK_COLS} FROM packs WHERE job=?1 AND project=?2 AND state='open' ORDER BY id DESC LIMIT 1"),
            params![job, project],
            pack_from_row,
        )
        .optional()?)
}

/// Every open pack of a job (one per project it wrote to).
pub fn open_packs_of_job(c: &Connection, job: &str) -> Result<Vec<Pack>> {
    let mut st = c.prepare(&format!("SELECT {PACK_COLS} FROM packs WHERE job=?1 AND state='open' ORDER BY id"))?;
    Ok(st.query_map([job], pack_from_row)?.collect::<rusqlite::Result<_>>()?)
}

pub fn set_pack_state(c: &Connection, id: i64, state: PackState) -> Result<()> {
    let col = match state {
        PackState::Sealed => Some("sealed_at"),
        PackState::Protected => Some("protected_at"),
        _ => None,
    };
    c.execute("UPDATE packs SET state=?1 WHERE id=?2", params![state.as_str(), id])?;
    if let Some(col) = col {
        c.execute(&format!("UPDATE packs SET {col}=?1 WHERE id=?2"), params![now_secs(), id])?;
    }
    Ok(())
}

pub fn set_pack_check(c: &Connection, id: i64, result: &str) -> Result<()> {
    c.execute("UPDATE packs SET checked_at=?1, check_result=?2 WHERE id=?3", params![now_secs(), result, id])?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PackEntry {
    pub path: String,
    pub kind: NodeKind,
    pub sha256: Option<Digest>,
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: u32,
    /// Tar member holding the bytes; `None` for folders, symlinks, and dedup references.
    pub member: Option<String>,
    /// Offset of the file within the member's decompressed stream (solid blocks).
    pub offset: Option<u64>,
    pub target: Option<String>,
    /// For deduplicated files: the path of the identical file whose bytes are stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_as: Option<String>,
}

pub fn insert_pack_entry(c: &Connection, pack_id: i64, e: &PackEntry, blob_id: Option<i64>) -> Result<()> {
    c.execute(
        "INSERT INTO pack_entries(pack_id, path, kind, sha256, size, mtime_ns, mode, blob_id, blob_off, target, same_as) \
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            pack_id,
            e.path,
            e.kind.code(),
            e.sha256.as_ref().map(|d| d.0.to_vec()),
            e.size as i64,
            e.mtime_ns,
            e.mode as i64,
            blob_id,
            e.offset.map(|o| o as i64),
            e.target,
            e.same_as
        ],
    )?;
    Ok(())
}

/// Path of a stored (not deduplicated) copy of this content in a project, as recorded in its pack.
pub fn stored_path(c: &Connection, project: &str, sha: &Digest) -> Result<Option<String>> {
    Ok(c
        .query_row(
            "SELECT e.path FROM pack_entries e JOIN packs p ON p.id = e.pack_id \
             WHERE e.sha256=?1 AND e.blob_id IS NOT NULL AND ifnull(p.project, '')=?2 ORDER BY e.id LIMIT 1",
            params![&sha.0[..], project],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn pack_entries(c: &Connection, pack_id: i64) -> Result<Vec<PackEntry>> {
    let mut st = c.prepare(
        "SELECT e.path, e.kind, e.sha256, e.size, e.mtime_ns, e.mode, b.member, e.blob_off, e.target, e.same_as \
         FROM pack_entries e LEFT JOIN blobs b ON b.id = e.blob_id WHERE e.pack_id=?1 ORDER BY e.id",
    )?;
    let rows = st.query_map([pack_id], |r| {
        let kind: String = r.get(1)?;
        let sha: Option<Vec<u8>> = r.get(2)?;
        Ok(PackEntry {
            path: r.get(0)?,
            kind: NodeKind::from_code(&kind).unwrap_or(NodeKind::File),
            sha256: sha.and_then(|b| Digest::from_slice(&b)),
            size: r.get::<_, i64>(3)? as u64,
            mtime_ns: r.get(4)?,
            mode: r.get::<_, i64>(5)? as u32,
            member: r.get(6)?,
            offset: r.get::<_, Option<i64>>(7)?.map(|o| o as u64),
            target: r.get(8)?,
            same_as: r.get(9)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat() -> (tempfile::TempDir, Catalog) {
        let d = tempfile::tempdir().unwrap();
        let c = Catalog::open(&d.path().join("catalog.db")).unwrap();
        (d, c)
    }

    fn add_content(c: &Connection, tag: &[u8], size: u64) -> i64 {
        c.execute("INSERT OR IGNORE INTO packs(id, name, state, created) VALUES(1, 'p', 'open', 0)", []).unwrap();
        let b = insert_blob(c, 1, "m", "f", 0, 10, size).unwrap();
        insert_content(c, "", &Digest::of(tag), size, b, 0).unwrap()
    }

    #[test]
    fn tree_totals_follow_moves_and_trash() {
        let (_d, cat) = cat();
        let c = cat.conn();
        let a = mkdir_p(c, &VPath::parse("/A/sub").unwrap()).unwrap();
        let b = mkdir_p(c, &VPath::parse("/B").unwrap()).unwrap();
        let x = add_content(c, b"x", 100);
        let f = insert_file(c, &NewFile { parent: a, name: "f.bin", content_id: x, size: 100, mtime_ns: 1, mode: 0o644 }).unwrap();
        insert_file(c, &NewFile { parent: b, name: "dup.bin", content_id: x, size: 100, mtime_ns: 1, mode: 0o644 }).unwrap();

        let root = entry(c, ROOT_ID).unwrap();
        assert_eq!((root.size, root.files), (200, 2));
        let a_top = resolve(c, &VPath::parse("/A").unwrap()).unwrap().unwrap();
        assert_eq!((a_top.size, a_top.files), (100, 1));

        move_node(c, f, b, "moved.bin").unwrap();
        assert_eq!(resolve(c, &VPath::parse("/A").unwrap()).unwrap().unwrap().size, 0);
        assert_eq!(resolve(c, &VPath::parse("/B").unwrap()).unwrap().unwrap().files, 2);
        assert!(move_node(c, a_top.id, a, "loop").is_err());

        trash(c, f).unwrap();
        assert_eq!(entry(c, ROOT_ID).unwrap().files, 1);
        assert!(resolve(c, &VPath::parse("/B/moved.bin").unwrap()).unwrap().is_none());
        let back = restore(c, f, None).unwrap();
        assert_eq!(back.to_string(), "/B/moved.bin");
        assert_eq!(entry(c, ROOT_ID).unwrap().files, 2);

        trash(c, b).unwrap();
        assert_eq!(purge_trash(c, now_secs() + 1).unwrap(), 1);
        let refs: i64 = c.query_row("SELECT refs FROM contents WHERE id=?1", [x], |r| r.get(0)).unwrap();
        assert_eq!(refs, 0);
        assert_eq!(entry(c, ROOT_ID).unwrap().files, 0);
    }

    fn file(c: &Connection, path: &str, size: u64, mtime_secs: i64) -> i64 {
        let p = VPath::parse(path).unwrap();
        let dir = mkdir_p(c, &p.parent().unwrap()).unwrap();
        let cid = add_content(c, path.as_bytes(), size);
        insert_file(
            c,
            &NewFile { parent: dir, name: p.name().unwrap(), content_id: cid, size, mtime_ns: mtime_secs * 1_000_000_000, mode: 0o644 },
        )
        .unwrap()
    }

    fn names(c: &Connection, q: &str) -> Vec<String> {
        let mut v: Vec<String> = search(c, q, 100).unwrap().into_iter().map(|(p, _)| p.to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn word_search_across_names_and_folders() {
        let (_d, cat) = cat();
        let c = cat.conn();
        file(c, "/Projects/Charity_Dairy/raw/run1_Sample.fastq.gz", 3_000_000_000, 1_700_000_000);
        file(c, "/Projects/Charity_Dairy/analysis/methylation_calls.csv", 5_000_000, 1_710_000_000);
        file(c, "/Projects/CharityDairy2023/notes.txt", 2_000, 1_600_000_000);
        file(c, "/Other/Données/résumé.pdf", 90_000, 1_650_000_000);

        // Words may be in the name or any folder above; each is a prefix.
        assert_eq!(names(c, "dairy fastq"), ["/Projects/Charity_Dairy/raw/run1_Sample.fastq.gz"]);
        assert_eq!(names(c, "meth"), ["/Projects/Charity_Dairy/analysis/methylation_calls.csv"]);
        // camelCase and digits split; accents ignored.
        assert_eq!(names(c, "2023 notes"), ["/Projects/CharityDairy2023/notes.txt"]);
        assert_eq!(names(c, "donnees resume"), ["/Other/Données/résumé.pdf"]);
        // A folder matches by its own name; names rank above folder matches.
        let hits = search(c, "charity", 10).unwrap();
        assert_eq!(hits[0].1.kind, NodeKind::Dir);
        // Filters, alone or with words.
        assert_eq!(names(c, "type:csv"), ["/Projects/Charity_Dairy/analysis/methylation_calls.csv"]);
        assert_eq!(names(c, "type:fastq"), ["/Projects/Charity_Dairy/raw/run1_Sample.fastq.gz"]);
        assert_eq!(names(c, "charity >1GB is:file"), ["/Projects/Charity_Dairy/raw/run1_Sample.fastq.gz"]);
        assert_eq!(names(c, "charity is:file before:2021"), ["/Projects/CharityDairy2023/notes.txt"]);
        assert_eq!(names(c, "is:folder raw"), ["/Projects/Charity_Dairy/raw"]);
        assert_eq!(names(c, "\"Charity Dairy\" raw is:folder"), ["/Projects/Charity_Dairy/raw"]);
        assert!(names(c, "nothing-like-this").is_empty());
        assert!(names(c, "   ").is_empty());
        // Awkward input doesn't break the query.
        assert!(search(c, "\" * AND OR ( ^", 10).is_ok());
    }

    #[test]
    fn index_follows_renames_trash_and_restore() {
        let (_d, cat) = cat();
        let c = cat.conn();
        file(c, "/Lab/2019_pilot/reads.fastq", 10, 0);
        let pilot = resolve(c, &VPath::parse("/Lab/2019_pilot").unwrap()).unwrap().unwrap();
        move_node(c, pilot.id, ROOT_ID, "Holstein_study").unwrap();
        assert_eq!(names(c, "holstein reads"), ["/Holstein_study/reads.fastq"]);
        assert!(names(c, "pilot").is_empty());
        trash(c, pilot.id).unwrap();
        assert!(names(c, "reads").is_empty(), "trashed items aren't found");
        restore(c, pilot.id, None).unwrap();
        assert_eq!(names(c, "reads"), ["/Holstein_study/reads.fastq"]);
        let f = resolve(c, &VPath::parse("/Holstein_study/reads.fastq").unwrap()).unwrap().unwrap();
        delete_subtree(c, f.id).unwrap();
        assert!(names(c, "reads").is_empty());
    }

    /// The schema of version 2 catalogs (before projects).
    const SCHEMA_V2: &str = r#"
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS packs(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL UNIQUE,
  job TEXT,
  state TEXT NOT NULL,
  bytes INTEGER NOT NULL DEFAULT 0,
  created INTEGER NOT NULL,
  sealed_at INTEGER,
  protected_at INTEGER,
  checked_at INTEGER,
  check_result TEXT
);
CREATE INDEX IF NOT EXISTS packs_job ON packs(job, state);
CREATE TABLE IF NOT EXISTS blobs(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  pack_id INTEGER NOT NULL REFERENCES packs(id),
  member TEXT NOT NULL,
  kind TEXT NOT NULL,
  data_off INTEGER NOT NULL,
  data_len INTEGER NOT NULL,
  raw_len INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS blobs_pack ON blobs(pack_id);
CREATE TABLE IF NOT EXISTS contents(
  id INTEGER PRIMARY KEY,
  sha256 BLOB NOT NULL UNIQUE,
  size INTEGER NOT NULL,
  blob_id INTEGER NOT NULL REFERENCES blobs(id),
  blob_off INTEGER NOT NULL,
  refs INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS contents_size ON contents(size);
CREATE INDEX IF NOT EXISTS contents_blob ON contents(blob_id);
CREATE TABLE IF NOT EXISTS nodes(
  id INTEGER PRIMARY KEY,
  parent INTEGER REFERENCES nodes(id),
  name TEXT NOT NULL,
  kind TEXT NOT NULL,
  content_id INTEGER REFERENCES contents(id),
  target TEXT,
  size INTEGER NOT NULL DEFAULT 0,
  files INTEGER NOT NULL DEFAULT 0,
  mtime_ns INTEGER NOT NULL DEFAULT 0,
  mode INTEGER NOT NULL DEFAULT 0,
  created INTEGER NOT NULL,
  trashed_at INTEGER,
  trash_origin TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS nodes_child ON nodes(parent, name);
CREATE INDEX IF NOT EXISTS nodes_content ON nodes(content_id);
CREATE INDEX IF NOT EXISTS nodes_name ON nodes(name COLLATE NOCASE);
CREATE TABLE IF NOT EXISTS pack_entries(
  id INTEGER PRIMARY KEY,
  pack_id INTEGER NOT NULL REFERENCES packs(id),
  path TEXT NOT NULL,
  kind TEXT NOT NULL,
  sha256 BLOB,
  size INTEGER NOT NULL DEFAULT 0,
  mtime_ns INTEGER NOT NULL DEFAULT 0,
  mode INTEGER NOT NULL DEFAULT 0,
  blob_id INTEGER REFERENCES blobs(id),
  blob_off INTEGER,
  target TEXT,
  same_as TEXT
);
CREATE INDEX IF NOT EXISTS pack_entries_pack ON pack_entries(pack_id);
CREATE INDEX IF NOT EXISTS pack_entries_sha ON pack_entries(sha256);
CREATE VIRTUAL TABLE IF NOT EXISTS name_index USING fts5(words, folders, tokenize = "unicode61 remove_diacritics 2", prefix = '2 3 4');
INSERT OR IGNORE INTO nodes(id, parent, name, kind, created) VALUES (1, NULL, '', 'd', 0);
INSERT OR IGNORE INTO nodes(id, parent, name, kind, created) VALUES (2, NULL, '.trash', 'd', 0);
"#;

    /// A catalog made by an older version: one file at /A/sequencing_run.bam.
    fn old_catalog(path: &Path, version: i64) {
        let c = Connection::open(path).unwrap();
        c.execute_batch(SCHEMA_V2).unwrap();
        c.execute_batch(
            "INSERT INTO packs(id, name, state, created) VALUES(1, 'p', 'sealed', 0);
             INSERT INTO blobs(id, pack_id, member, kind, data_off, data_len, raw_len) VALUES(1, 1, 'm', 'f', 0, 10, 10);
             INSERT INTO contents(id, sha256, size, blob_id, blob_off, refs) VALUES(1, x'00', 10, 1, 0, 1);
             INSERT INTO nodes(id, parent, name, kind, size, files, created) VALUES(3, 1, 'A', 'd', 10, 1, 0);
             INSERT INTO nodes(id, parent, name, kind, content_id, size, files, created) VALUES(4, 3, 'sequencing_run.bam', 'f', 1, 10, 1, 0);
             UPDATE nodes SET size = 10, files = 1 WHERE id = 1;",
        )
        .unwrap();
        if version >= 2 {
            c.execute_batch("INSERT INTO name_index(rowid, words, folders) VALUES(3, 'a', ''), (4, 'sequencing run bam', 'a');").unwrap();
        } else {
            c.execute_batch("DROP TABLE name_index;").unwrap();
        }
        c.execute("INSERT INTO meta(key, value) VALUES('schema_version', ?1)", [version.to_string()]).unwrap();
    }

    #[test]
    fn old_catalogs_are_upgraded() {
        for version in [1, 2] {
            let d = tempfile::tempdir().unwrap();
            let path = d.path().join("catalog.db");
            old_catalog(&path, version);
            let cat = Catalog::open(&path).unwrap();
            let c = cat.conn();
            assert_eq!(cat.meta_get("schema_version").unwrap().as_deref(), Some("4"));
            assert_eq!(names(c, "sequencing"), ["/A/sequencing_run.bam"]);
            // Old data belongs to no project and deduplicates only with other old data.
            let f = resolve(c, &VPath::parse("/A/sequencing_run.bam").unwrap()).unwrap().unwrap();
            assert!(f.project.is_none() && !f.in_project);
            assert!(content_by_hash(c, "", &Digest::from_slice(&[0]).unwrap_or(Digest([0; 32]))).is_ok());
            assert!(size_present(c, "", 10).unwrap());
            assert!(!size_present(c, "p1", 10).unwrap());
            // Foreign keys still hold after rebuilding the contents table.
            let fk: Vec<String> = c.prepare("PRAGMA foreign_key_check").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
            assert!(fk.is_empty(), "{fk:?}");
            drop(cat);
            Catalog::open(&path).unwrap();
        }
    }

    #[test]
    fn projects_scope_contents_and_are_inherited() {
        let (_d, cat) = cat();
        let c = cat.conn();
        let org = mkdir_p(c, &VPath::parse("/Lab/2024").unwrap()).unwrap();
        let (root, p) = create_project(c, org, "Dairy", 0, 0o755).unwrap();
        assert_eq!(create_project(c, org, "Dairy", 0, 0o755).unwrap(), (root, p.clone()));
        let sub = mkdir(c, root, "raw", 0, 0o755).unwrap();
        assert_eq!(entry(c, sub).unwrap().project.as_deref(), Some(p.as_str()));
        assert!(entry(c, sub).unwrap().in_project);
        let top = entry(c, root).unwrap();
        assert!(top.is_project && !top.in_project);
        assert!(entry(c, org).unwrap().project.is_none());
        // No projects inside projects, and an organizing folder can't become one.
        assert!(create_project(c, sub, "Nested", 0, 0o755).is_err());
        assert!(create_project(c, entry(c, org).unwrap().parent.unwrap(), "2024", 0, 0o755).is_err());
        assert_eq!(project_for(c, &VPath::parse("/Lab/2024/Dairy/raw/new/file").unwrap()).unwrap().as_deref(), Some(p.as_str()));
        assert_eq!(project_for(c, &VPath::parse("/Lab/2024/Other").unwrap()).unwrap(), None);
        assert_eq!(projects_under(c, ROOT_ID).unwrap(), vec![p.clone()]);
        // The same content in two projects is two rows.
        c.execute("INSERT INTO packs(id, name, state, created) VALUES(1, 'p', 'open', 0)", []).unwrap();
        let b = insert_blob(c, 1, "m", "f", 0, 10, 10).unwrap();
        let d = Digest::of(b"same");
        insert_content(c, &p, &d, 10, b, 0).unwrap();
        assert!(content_by_hash(c, &p, &d).unwrap().is_some());
        assert!(content_by_hash(c, "other", &d).unwrap().is_none());
        insert_content(c, "other", &d, 10, b, 0).unwrap();
        // Purging a project removes its project record.
        trash(c, root).unwrap();
        purge_trash(c, now_secs() + 1).unwrap();
        assert!(project_root(c, &p).unwrap().is_none());
    }

    #[test]
    fn reserved_name_rejected() {
        let (_d, cat) = cat();
        assert!(mkdir(cat.conn(), ROOT_ID, ".archive", 0, 0o755).is_err());
    }
}
