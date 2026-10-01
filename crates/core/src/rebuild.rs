//! Rebuild a catalog from the packs alone, for when `catalog.db` and its
//! snapshots are all lost. Each sealed pack's manifest says what it holds, at
//! which path, and which project it belongs to; applying manifests in pack
//! order reproduces the tree. A project that was renamed or moved between
//! packs ends up where its newest pack says. Other renames, moves, and the
//! trash are catalog-only, so they are not recovered (restore a catalog
//! snapshot, or see INDEX.tsv, for those).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::catalog::{self as cat, Catalog, NodeKind};
use crate::error::{Error, Result};
use crate::seekable::SeekTable;
use crate::store::{Config, Manifest, ProjectRef};
use crate::tar;
use crate::vpath::VPath;

#[derive(Clone, Debug, Default, Serialize)]
pub struct RebuildReport {
    pub packs: usize,
    pub files: usize,
    pub folders: usize,
    /// Packs without a manifest (never sealed), with the reason.
    pub skipped_packs: Vec<(String, String)>,
    /// Deduplicated files whose stored copy wasn't found in any pack.
    pub missing: Vec<String>,
}

fn pack_files(root: &Path) -> Result<Vec<(i64, String)>> {
    let mut out = Vec::new();
    let packs = root.join("packs");
    for month in fs::read_dir(&packs)?.flatten() {
        if !month.path().is_dir() {
            continue;
        }
        for f in fs::read_dir(month.path())?.flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            let Some(num) = name.strip_prefix("pack-").and_then(|n| n.strip_suffix(".tar")) else { continue };
            let Ok(id) = num.parse::<i64>() else { continue };
            out.push((id, format!("{}/{name}", month.file_name().to_string_lossy())));
        }
    }
    out.sort();
    Ok(out)
}

/// Remove whatever is at `path` so a later upload's entry replaces it.
fn clear(c: &Connection, path: &VPath) -> Result<()> {
    if let Some(e) = cat::resolve(c, path)? {
        if e.id != cat::ROOT_ID {
            cat::delete_subtree(c, e.id)?;
        }
    }
    Ok(())
}

/// Put project `pr`'s top folder at its recorded path, creating the project
/// or moving it there. Anything else in the way is renamed "name (2)".
fn place_project(c: &Connection, pr: &ProjectRef) -> Result<()> {
    let path = VPath::parse(&pr.path)?;
    let name = path.name().ok_or_else(|| Error::Corrupt(format!("project {} has no folder name", pr.id)))?;
    let root = cat::project_root(c, &pr.id)?;
    if let Some(r) = root {
        if cat::path_of(c, r)?.0 == path {
            return Ok(());
        }
    }
    let parent = cat::mkdir_p(c, &path.parent().unwrap_or_default())?;
    if let Some(other) = cat::child(c, parent, name)? {
        let mut n = 2;
        while cat::child(c, parent, &crate::util::numbered_name(name, n))?.is_some() {
            n += 1;
        }
        cat::move_node(c, other.id, parent, &crate::util::numbered_name(name, n))?;
    }
    match root {
        Some(r) => cat::move_node(c, r, parent, name)?,
        None => {
            let node = cat::mkdir(c, parent, name, 0, 0o755)?;
            cat::adopt_project(c, node, &pr.id)?;
        }
    }
    Ok(())
}

/// Write a new catalog at `out` from the packs under `root`.
pub fn rebuild(root: &Path, out: &Path) -> Result<RebuildReport> {
    if out.exists() {
        return Err(Error::AlreadyExists(out.display().to_string()));
    }
    let cfg: Config = serde_json::from_slice(&fs::read(root.join("archive.json"))?)?;
    let mut catalog = Catalog::open(out)?;
    catalog.meta_set("archive_id", &cfg.archive_id)?;
    catalog.meta_set("rebuilt_from_packs", &crate::util::now_secs().to_string())?;
    let mut report = RebuildReport::default();

    for (id, name) in pack_files(root)? {
        let path: PathBuf = root.join("packs").join(&name);
        let mut f = File::open(&path)?;
        let len = f.metadata()?.len();
        let members = match tar::list(&mut f, 0, Some(len)) {
            Ok(m) => m,
            Err(e) => {
                report.skipped_packs.push((name, format!("unreadable: {e} (repair it with par2 first)")));
                continue;
            }
        };
        let by_path: HashMap<&str, &tar::Entry> = members.iter().map(|m| (m.path.as_str(), m)).collect();
        let Some(mf) = by_path.get(".archive/MANIFEST.json") else {
            report.skipped_packs.push((name, "no manifest (the pack was never sealed)".into()));
            continue;
        };
        let mut raw = vec![0u8; mf.size as usize];
        f.seek(SeekFrom::Start(mf.data_off))?;
        f.read_exact(&mut raw)?;
        let manifest: Manifest = serde_json::from_slice(&raw)?;
        let protected = crate::par2::recovery_files(&path)?.iter().any(|p| p.extension().is_some_and(|x| x == "par2"));

        let tx = catalog.conn_mut().transaction()?;
        let project = manifest.project.as_ref().map(|p| p.id.clone()).unwrap_or_default();
        tx.execute(
            "INSERT INTO packs(id, name, state, bytes, created, sealed_at, project) VALUES(?1, ?2, ?3, ?4, ?5, ?5, ?6)",
            params![
                id,
                name,
                if protected { "protected" } else { "sealed" },
                len as i64,
                manifest.sealed_at,
                manifest.project.as_ref().map(|p| &p.id)
            ],
        )?;
        if let Some(pr) = &manifest.project {
            place_project(&tx, pr)?;
        }
        let mut blobs: HashMap<String, i64> = HashMap::new();
        for e in &manifest.entries {
            let vpath = VPath::parse(&e.path)?;
            match e.kind {
                NodeKind::Dir => {
                    let dir = cat::mkdir_p(&tx, &vpath)?;
                    tx.execute("UPDATE nodes SET mtime_ns=?1, mode=?2 WHERE id=?3", params![e.mtime_ns, e.mode as i64, dir])?;
                    report.folders += 1;
                }
                NodeKind::Symlink => {
                    clear(&tx, &vpath)?;
                    let parent = cat::mkdir_p(&tx, &vpath.parent().unwrap_or_default())?;
                    cat::insert_symlink(&tx, parent, vpath.name().unwrap_or_default(), e.target.as_deref().unwrap_or(""), e.mtime_ns)?;
                }
                NodeKind::File => {
                    let Some(sha) = e.sha256 else { continue };
                    let content = match &e.member {
                        Some(member) => {
                            let blob = match blobs.get(member) {
                                Some(b) => *b,
                                None => {
                                    let m = by_path
                                        .get(member.as_str())
                                        .ok_or_else(|| Error::Corrupt(format!("{name}: manifest names missing member {member}")))?;
                                    let table = SeekTable::read(&mut f, m.data_off, m.size)?;
                                    let kind = if member.starts_with(".archive/solid-") { "s" } else { "f" };
                                    let b = cat::insert_blob(&tx, id, member, kind, m.data_off, m.size, table.raw_len())?;
                                    blobs.insert(member.clone(), b);
                                    b
                                }
                            };
                            match cat::content_by_hash(&tx, &project, &sha)? {
                                Some((c, _)) => c,
                                None => cat::insert_content(&tx, &project, &sha, e.size, blob, e.offset.unwrap_or(0))?,
                            }
                        }
                        None => match cat::content_by_hash(&tx, &project, &sha)? {
                            Some((c, _)) => c,
                            None => {
                                report.missing.push(e.path.clone());
                                continue;
                            }
                        },
                    };
                    clear(&tx, &vpath)?;
                    let parent = cat::mkdir_p(&tx, &vpath.parent().unwrap_or_default())?;
                    let nf = cat::NewFile {
                        parent,
                        name: vpath.name().unwrap_or_default(),
                        content_id: content,
                        size: e.size,
                        mtime_ns: e.mtime_ns,
                        mode: e.mode,
                    };
                    cat::insert_file(&tx, &nf)?;
                    report.files += 1;
                }
            }
            cat::insert_pack_entry(&tx, id, e, e.member.as_ref().and_then(|m| blobs.get(m).copied()))?;
        }
        tx.commit()?;
        report.packs += 1;
    }
    // AUTOINCREMENT keeps new pack ids above the explicit ones inserted here.
    Ok(report)
}
