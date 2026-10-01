//! Background maintenance: protect sealed packs with PAR2, scrub and repair,
//! purge the trash, and summarize a folder for the Get info panel.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::time::Instant;

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::catalog::{self as cat, NodeKind, PackState};
use crate::error::{Error, Result};
use crate::hash::{Digest, hash_reader};
use crate::par2::{self, Par2, Verify};
use crate::seekable::RangeReader;
use crate::store::Store;
use crate::util::now_secs;
use crate::vpath::VPath;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackResult {
    pub pack: String,
    pub outcome: String,
}

fn out_of_time(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|d| Instant::now() >= d)
}

/// Create and verify PAR2 recovery data for sealed packs, stopping at `deadline`.
pub fn protect(store: &Store, par2: &Par2, deadline: Option<Instant>, on: &mut dyn FnMut(&PackResult)) -> Result<Vec<PackResult>> {
    let mut out = Vec::new();
    for p in cat::packs_in_state(store.conn(), PackState::Sealed)? {
        if out_of_time(deadline) {
            break;
        }
        let path = store.pack_path(&p.name);
        let outcome = match par2.create(&path, store.config().par2_redundancy_percent).and_then(|_| par2.verify(&path)) {
            Ok(Verify::Ok) => {
                cat::set_pack_state(store.conn(), p.id, PackState::Protected)?;
                cat::set_pack_check(store.conn(), p.id, "ok")?;
                "protected".to_string()
            }
            Ok(v) => {
                // Freshly created recovery data that doesn't verify means the pack
                // changed underneath us; leave it sealed and report it.
                let _ = par2::remove_recovery_files(&path);
                format!("recovery data did not verify ({v:?}); will retry")
            }
            Err(e) => format!("failed: {e}"),
        };
        let r = PackResult { pack: p.name, outcome };
        on(&r);
        out.push(r);
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScrubOptions {
    /// Only check packs last checked before this time (or never).
    pub due_before: Option<i64>,
    /// Stop starting new packs after this instant.
    pub deadline: Option<Instant>,
}

/// Verify packs against their recovery data, repairing damage. Packs without
/// recovery data yet get a full decompress-and-hash check. Least recently
/// checked packs go first.
pub fn scrub(store: &Store, par2: &Par2, opts: ScrubOptions, on: &mut dyn FnMut(&PackResult)) -> Result<Vec<PackResult>> {
    let mut out = Vec::new();
    let mut packs = cat::packs(store.conn())?;
    packs.sort_by_key(|p| p.checked_at.unwrap_or(0));
    for p in packs {
        if out_of_time(opts.deadline) {
            break;
        }
        if let (Some(due), Some(checked)) = (opts.due_before, p.checked_at) {
            if checked >= due {
                continue;
            }
        }
        let path = store.pack_path(&p.name);
        let outcome = match p.state {
            PackState::Open => continue,
            PackState::Sealed => match deep_check(store, p.id) {
                Ok(()) => {
                    cat::set_pack_check(store.conn(), p.id, "ok (no recovery data yet)")?;
                    "ok (no recovery data yet)".to_string()
                }
                Err(e) => {
                    cat::set_pack_state(store.conn(), p.id, PackState::Damaged)?;
                    cat::set_pack_check(store.conn(), p.id, &format!("damaged: {e}"))?;
                    format!("DAMAGED, no recovery data: {e}")
                }
            },
            PackState::Protected | PackState::Damaged => match par2.verify(&path) {
                Ok(Verify::Ok) => {
                    if p.state == PackState::Damaged {
                        cat::set_pack_state(store.conn(), p.id, PackState::Protected)?;
                    }
                    cat::set_pack_check(store.conn(), p.id, "ok")?;
                    "ok".to_string()
                }
                Ok(Verify::Repairable) => {
                    if par2.repair(&path)? && deep_check(store, p.id).is_ok() {
                        cat::set_pack_state(store.conn(), p.id, PackState::Protected)?;
                        cat::set_pack_check(store.conn(), p.id, "repaired")?;
                        "damage found and repaired".to_string()
                    } else {
                        cat::set_pack_state(store.conn(), p.id, PackState::Damaged)?;
                        cat::set_pack_check(store.conn(), p.id, "repair failed")?;
                        "DAMAGED: repair failed".to_string()
                    }
                }
                Ok(Verify::Unrepairable) => {
                    cat::set_pack_state(store.conn(), p.id, PackState::Damaged)?;
                    cat::set_pack_check(store.conn(), p.id, "damaged beyond recovery data")?;
                    "DAMAGED beyond what recovery data can fix".to_string()
                }
                Err(e) => format!("check failed: {e}"),
            },
        };
        let r = PackResult { pack: p.name, outcome };
        on(&r);
        out.push(r);
    }
    Ok(out)
}

/// Decompress every blob in a pack and check each file's SHA-256.
pub fn deep_check(store: &Store, pack_id: i64) -> Result<()> {
    let c = store.conn();
    let pack = cat::pack(c, pack_id)?;
    let path = store.pack_path(&pack.name);
    let mut st = c.prepare(
        "SELECT b.id, b.data_off, b.data_len, c.sha256, c.blob_off, c.size FROM blobs b JOIN contents c ON c.blob_id = b.id \
         WHERE b.pack_id = ?1 ORDER BY b.id, c.blob_off",
    )?;
    let rows: Vec<(i64, u64, u64, Vec<u8>, u64, u64)> = st
        .query_map([pack_id], |r| {
            Ok((
                r.get(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
                r.get(3)?,
                r.get::<_, i64>(4)? as u64,
                r.get::<_, i64>(5)? as u64,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (_, off, len, sha, blob_off, size) in rows {
        let f = File::open(&path)?;
        let rr = RangeReader::new(f, off, len, blob_off, size)?;
        let (got, n) = hash_reader(rr).map_err(|e| Error::Corrupt(e.to_string()))?;
        if Some(got) != Digest::from_slice(&sha) || n != size {
            return Err(Error::Corrupt(format!(
                "content {} in {} does not match its checksum",
                Digest::from_slice(&sha).map(|d| d.to_hex()).unwrap_or_default(),
                pack.name
            )));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PurgeReport {
    pub purged_items: usize,
    pub packs_deleted: Vec<String>,
    pub bytes_freed: u64,
}

/// Permanently delete trash entries older than the archive's trash period, then
/// delete packs none of whose contents are referenced any more.
pub fn purge(store: &mut Store, older_than_days: Option<u32>) -> Result<PurgeReport> {
    let days = older_than_days.unwrap_or(store.config().trash_days) as i64;
    let mut report = PurgeReport::default();
    {
        let tx = store.catalog_mut().conn_mut().transaction()?;
        report.purged_items = cat::purge_trash(&tx, now_secs() - days * 86_400)?;
        tx.commit()?;
    }
    // Packs whose blobs all hold only unreferenced content.
    let dead: Vec<(i64, String, u64)> = {
        let c = store.conn();
        let mut st = c.prepare(
            "SELECT p.id, p.name, p.bytes FROM packs p WHERE p.state <> 'open' \
             AND EXISTS (SELECT 1 FROM blobs b WHERE b.pack_id = p.id) \
             AND NOT EXISTS (SELECT 1 FROM blobs b JOIN contents c ON c.blob_id = b.id WHERE b.pack_id = p.id AND c.refs > 0)",
        )?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? as u64)))?.collect::<rusqlite::Result<_>>()?
    };
    for (id, name, bytes) in dead {
        let path = store.pack_path(&name);
        {
            let tx = store.catalog_mut().conn_mut().transaction()?;
            tx.execute("DELETE FROM contents WHERE blob_id IN (SELECT id FROM blobs WHERE pack_id=?1)", [id])?;
            tx.execute("DELETE FROM pack_entries WHERE pack_id=?1", [id])?;
            tx.execute("DELETE FROM blobs WHERE pack_id=?1", [id])?;
            tx.execute("DELETE FROM packs WHERE id=?1", [id])?;
            tx.commit()?;
        }
        par2::remove_recovery_files(&path)?;
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        report.bytes_freed += bytes;
        report.packs_deleted.push(name);
    }
    Ok(report)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protection {
    /// Every pack holding this data has verified recovery data.
    Protected,
    /// Stored and verified, but recovery data is still being prepared.
    Pending,
    /// Some data is damaged beyond what recovery data could fix.
    Damaged,
    /// Nothing stored (empty folder).
    None,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Info {
    pub path: VPath,
    pub kind: NodeKind,
    pub files: u64,
    pub folders: u64,
    /// Total size of all files as users see them.
    pub original_bytes: u64,
    /// Approximate space the data takes on the server (compressed, deduplicated).
    pub stored_bytes: u64,
    /// Bytes not stored again because identical content already existed.
    pub duplicate_bytes: u64,
    pub archived_at: i64,
    pub protection: Protection,
    /// Oldest successful integrity check across the packs involved.
    pub last_checked: Option<i64>,
}

/// Summary of a file or folder for the Get info panel.
pub fn info(store: &Store, path: &VPath) -> Result<Info> {
    let c = store.conn();
    let e = cat::resolve(c, path)?.ok_or_else(|| Error::NotFound(path.to_string()))?;
    let mut st = c.prepare(
        "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION ALL SELECT n.id FROM nodes n JOIN sub ON n.parent = sub.id) \
         SELECT n.kind, n.content_id, n.size, n.created, b.kind, b.data_len, b.raw_len, p.id, p.state, p.checked_at \
         FROM sub JOIN nodes n ON n.id = sub.id \
         LEFT JOIN contents c ON c.id = n.content_id LEFT JOIN blobs b ON b.id = c.blob_id LEFT JOIN packs p ON p.id = b.pack_id",
    )?;
    let mut rows = st.query(params![e.id])?;
    let (mut files, mut folders, mut original, mut stored, mut unique_bytes) = (0u64, 0u64, 0u64, 0f64, 0u64);
    let mut archived_at = e.created;
    let mut seen: HashSet<i64> = HashSet::new();
    let mut packs: HashMap<i64, (String, Option<i64>)> = HashMap::new();
    while let Some(r) = rows.next()? {
        let kind: String = r.get(0)?;
        let created: i64 = r.get(3)?;
        archived_at = archived_at.max(created);
        match kind.as_str() {
            "d" => folders += 1,
            "f" => {
                files += 1;
                let size = r.get::<_, i64>(2)? as u64;
                original += size;
                if let Some(cid) = r.get::<_, Option<i64>>(1)? {
                    if seen.insert(cid) {
                        unique_bytes += size;
                        let (bkind, dlen, rlen): (String, i64, i64) = (r.get(4)?, r.get(5)?, r.get(6)?);
                        stored += if bkind == "s" && rlen > 0 { dlen as f64 * size as f64 / rlen as f64 } else { dlen as f64 };
                        packs.insert(r.get(7)?, (r.get(8)?, r.get(9)?));
                    }
                }
            }
            _ => {}
        }
    }
    if e.kind == NodeKind::Dir {
        folders = folders.saturating_sub(1); // don't count the folder itself
    }
    let protection = if packs.is_empty() {
        Protection::None
    } else if packs.values().any(|(s, _)| s == "damaged") {
        Protection::Damaged
    } else if packs.values().all(|(s, _)| s == "protected") {
        Protection::Protected
    } else {
        Protection::Pending
    };
    let last_checked = if packs.values().all(|(_, t)| t.is_some()) { packs.values().filter_map(|(_, t)| *t).min() } else { None };
    Ok(Info {
        path: path.clone(),
        kind: e.kind,
        files,
        folders,
        original_bytes: original,
        stored_bytes: stored.round() as u64,
        duplicate_bytes: original - unique_bytes,
        archived_at,
        protection,
        last_checked,
    })
}
