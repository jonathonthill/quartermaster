//! Importing data from the earlier move2storage transfer tool.
//!
//! That tool moved each top-level folder in parts (`Name.part0001`, ...). On
//! the server, a part is either:
//! - **bundled**: `<part>.bundle.tar`, a tar holding `<part>.tar` (the files,
//!   each compressed with zstd as `<folder>/<path>.zst`), `<part>.tar.sha256`,
//!   and `MANIFEST.json` (each file's path, size, mtime, and SHA-256), with the
//!   bundle's own checksum in `<part>.bundle.tar.sha256` beside it; or
//! - **staged**: in the tool's job folder, `<part>/tree/<folder>/<path>.zst`
//!   with one receipt per file in `<part>/receipts/` (the same fields), for
//!   parts the tool never got to bundle. `allowed-parts.json` in the job
//!   folder names each part's folder.
//!
//! Either way, every file is checked against its recorded SHA-256 as it is
//! unpacked into a staging folder, and the files are then moved into the
//! archive like any other send: the folder becomes a project, and later parts
//! of the same folder are added to it. The staged copies are deleted only once
//! the archive has verified them. The original bundles and job folder are never
//! modified. A receipt records each finished part, so running the import again
//! skips it.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::api::Archive;
use crate::error::{Error, Result};
use crate::hash::{Digest, Hasher, hash_reader};
use crate::send::{self, Mode, SendOptions, SendReport};
use crate::tar::{self, Kind};
use crate::util::{self, now_secs};
use crate::vpath::VPath;

#[derive(Debug, Deserialize)]
struct Manifest {
    name: String,
    source_name: String,
    files: Vec<FileRecord>,
    #[serde(default)]
    directories: Vec<String>,
}

/// One file as the old tool recorded it (in a manifest or a receipt).
#[derive(Debug, Deserialize)]
struct FileRecord {
    path: String,
    size: u64,
    #[serde(default)]
    mtime_ns: i64,
    sha256: Digest,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Checking a bundle's checksum (reads the whole bundle).
    Checking { part: String },
    /// Unpacking and checking its files.
    Unpacking { part: String, files: u64, bytes: u64 },
    /// Sending the unpacked files into the archive.
    Sending { part: String, project: VPath },
    Progress { done: u64, total: u64 },
    /// Already imported (a receipt exists).
    AlreadyDone { part: String },
    /// Skipped, with the reason (for example, bundled instead of staged).
    Skipped { part: String, reason: String },
    Imported { part: String, files: u64 },
}

/// What happened to one part.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    /// The bundle file or the staged part folder.
    #[serde(alias = "bundle")]
    pub source: String,
    pub part: String,
    pub folder: String,
    pub project: String,
    pub files: u64,
    pub bytes: u64,
    /// "bundle", "inner tar", or "server receipts": what was checked before unpacking.
    pub checked: String,
    pub stored: u64,
    pub deduplicated: u64,
    pub identical: u64,
    pub imported_at: i64,
}

#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// Archive folder the projects go into.
    pub dest: VPath,
    /// Where parts are unpacked before sending (emptied as files are archived).
    pub staging: PathBuf,
    /// Where receipts of finished parts are kept.
    pub receipts: PathBuf,
    pub send: SendOptions,
}

/// Where one file's compressed bytes are.
enum Stored {
    /// A member of the bundle's inner tar: offset and length in the bundle.
    Member(u64, u64),
    /// A staged `.zst` file.
    File(PathBuf),
}

struct Item {
    rec: FileRecord,
    stored: Stored,
}

/// One part, checked as far as possible before anything is unpacked.
struct Plan {
    source: PathBuf,
    part: String,
    folder: String,
    dirs: Vec<String>,
    items: Vec<Item>,
    checked: &'static str,
}

/// Import bundles in name order. Stops at the first one that fails, leaving
/// its unpacked files in the staging folder for inspection.
pub fn import(archive: &mut dyn Archive, bundles: &[PathBuf], opts: &ImportOptions, on: &mut dyn FnMut(&Event)) -> Result<Vec<Receipt>> {
    let mut sorted = bundles.to_vec();
    sorted.sort();
    let mut out = Vec::new();
    for b in &sorted {
        let part = file_name(b).strip_suffix(".bundle.tar").map(str::to_string).ok_or_else(|| Error::InvalidPath(format!("{} isn't a .bundle.tar", b.display())))?;
        if let Some(r) = done(opts, &part, on)? {
            out.push(r);
            continue;
        }
        let plan = plan_bundle(b, &part, on)?;
        out.push(run(archive, plan, opts, on)?);
    }
    Ok(out)
}

/// Import every part the old tool staged but never bundled, from its job
/// folder, in name order. Bundled parts are skipped (import their bundles).
pub fn import_staged(archive: &mut dyn Archive, job: &Path, opts: &ImportOptions, on: &mut dyn FnMut(&Event)) -> Result<Vec<Receipt>> {
    let allowed: HashMap<String, String> = serde_json::from_slice(&fs::read(job.join("allowed-parts.json"))?)?;
    let mut parts: Vec<(&String, &String)> = allowed.iter().collect();
    parts.sort();
    let mut out = Vec::new();
    for (part, folder) in parts {
        let dir = job.join(part);
        if dir.join("complete.json").exists() {
            on(&Event::Skipped { part: part.clone(), reason: "bundled; import its .bundle.tar instead".into() });
            continue;
        }
        if !dir.join("receipts").is_dir() {
            on(&Event::Skipped { part: part.clone(), reason: "nothing was staged".into() });
            continue;
        }
        if let Some(r) = done(opts, part, on)? {
            out.push(r);
            continue;
        }
        let plan = plan_staged(&dir, part, folder, on)?;
        out.push(run(archive, plan, opts, on)?);
    }
    Ok(out)
}

fn done(opts: &ImportOptions, part: &str, on: &mut dyn FnMut(&Event)) -> Result<Option<Receipt>> {
    match fs::read(opts.receipts.join(format!("{part}.json"))) {
        Ok(bytes) => {
            on(&Event::AlreadyDone { part: part.to_string() });
            Ok(Some(serde_json::from_slice(&bytes)?))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// A recorded path must stay inside its folder.
fn safe_rel(p: &str) -> Result<PathBuf> {
    let path = Path::new(p);
    if p.is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)) {
        return Err(Error::Corrupt(format!("unsafe path recorded by the old tool: {p:?}")));
    }
    Ok(path.to_path_buf())
}

/// SHA-256 of `len` bytes at `off` in `f`.
fn hash_range(f: &mut File, off: u64, len: u64) -> Result<Digest> {
    f.seek(SeekFrom::Start(off))?;
    let (d, n) = hash_reader(f.take(len))?;
    if n != len {
        return Err(Error::Corrupt("bundle is shorter than its tar headers say".into()));
    }
    Ok(d)
}

/// The first hex word of a `.sha256` file.
fn read_sha_file(text: &str) -> Result<Digest> {
    text.split_whitespace().next().and_then(Digest::from_hex).ok_or_else(|| Error::Corrupt("unreadable .sha256 file".into()))
}

fn plan_bundle(bundle: &Path, part: &str, on: &mut dyn FnMut(&Event)) -> Result<Plan> {
    let bname = file_name(bundle);
    let fail = |what: String| Error::Corrupt(format!("{bname}: {what}"));
    let mut f = File::open(bundle)?;
    let len = f.metadata()?.len();
    let members = tar::list(&mut f, 0, Some(len))?;
    let by_name: HashMap<&str, &tar::Entry> = members.iter().map(|m| (m.path.as_str(), m)).collect();
    let get = |n: &str| by_name.get(n).copied().ok_or_else(|| fail(format!("no {n} inside (not a move2storage bundle?)")));
    let inner = get(&format!("{part}.tar"))?;
    let read_member = |f: &mut File, m: &tar::Entry| -> Result<Vec<u8>> {
        let mut buf = vec![0u8; m.size as usize];
        f.seek(SeekFrom::Start(m.data_off))?;
        f.read_exact(&mut buf)?;
        Ok(buf)
    };
    let manifest: Manifest = serde_json::from_slice(&read_member(&mut f, get("MANIFEST.json")?)?)?;
    if manifest.name != part {
        return Err(fail(format!("its manifest is for {}", manifest.name)));
    }
    let folder = manifest.source_name.clone();
    safe_rel(&folder)?;

    // The bundle's checksum, or failing that the inner tar's.
    on(&Event::Checking { part: part.to_string() });
    let outer_sha = bundle.with_file_name(format!("{bname}.sha256"));
    let checked = if let Ok(text) = fs::read_to_string(&outer_sha) {
        if hash_range(&mut f, 0, len)? != read_sha_file(&text)? {
            return Err(fail("doesn't match its .sha256 file".into()));
        }
        "bundle"
    } else {
        let want = read_sha_file(&String::from_utf8_lossy(&read_member(&mut f, get(&format!("{part}.tar.sha256"))?)?))?;
        if hash_range(&mut f, inner.data_off, inner.size)? != want {
            return Err(fail(format!("{part}.tar doesn't match its checksum")));
        }
        "inner tar"
    };

    // Every file in the manifest, and nothing else, must be in the inner tar.
    let mut expected: HashMap<String, FileRecord> = HashMap::new();
    for m in manifest.files {
        safe_rel(&m.path)?;
        let key = format!("{folder}/{}.zst", m.path.trim_start_matches("./"));
        if let Some(dup) = expected.insert(key, m) {
            return Err(fail(format!("{} is listed twice", dup.path)));
        }
    }
    let mut items = Vec::with_capacity(expected.len());
    let mut seen = HashSet::new();
    for m in tar::list(&mut f, inner.data_off, Some(inner.data_off + inner.size))? {
        match m.kind {
            Kind::Dir => {}
            Kind::File if expected.contains_key(m.path.as_str()) && seen.insert(m.path.clone()) => {
                let rec = expected.remove(&m.path).expect("checked above");
                items.push(Item { rec, stored: Stored::Member(m.data_off, m.size) });
            }
            _ => return Err(fail(format!("unexpected member {}", m.path))),
        }
    }
    if !expected.is_empty() {
        return Err(fail(format!("{} files in the manifest are missing from it", expected.len())));
    }
    Ok(Plan { source: bundle.to_path_buf(), part: part.to_string(), folder, dirs: manifest.directories, items, checked })
}

fn plan_staged(dir: &Path, part: &str, folder: &str, on: &mut dyn FnMut(&Event)) -> Result<Plan> {
    safe_rel(folder)?;
    let fail = |what: String| Error::Corrupt(format!("{part}: {what}"));
    let tree = dir.join("tree").join(folder);
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for e in fs::read_dir(dir.join("receipts"))? {
        let p = e?.path();
        if p.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let rec: FileRecord = serde_json::from_slice(&fs::read(&p)?).map_err(|e| fail(format!("{}: {e}", p.display())))?;
        let rel = safe_rel(&rec.path)?;
        if !seen.insert(rel.clone()) {
            return Err(fail(format!("{} has two receipts", rec.path)));
        }
        let zst = tree.join(format!("{}.zst", rel.display()));
        if !zst.is_file() {
            return Err(fail(format!("{} has a receipt but its staged file is missing", rec.path)));
        }
        items.push(Item { rec, stored: Stored::File(zst) });
    }
    items.sort_by(|a, b| a.rec.path.cmp(&b.rec.path));
    // Files still arriving when the tool stopped were never receipted, so
    // their originals were never deleted: send them from there instead.
    let partial = count_partial(&tree);
    if partial > 0 {
        on(&Event::Skipped { part: part.to_string(), reason: format!("{partial} unfinished files left out; their originals were never deleted") });
    }
    let dirs = match fs::read(dir.join("staged-manifest.json")) {
        Ok(b) => serde_json::from_slice::<Manifest>(&b)?.directories,
        Err(_) => Vec::new(),
    };
    Ok(Plan { source: dir.to_path_buf(), part: part.to_string(), folder: folder.to_string(), dirs, items, checked: "server receipts" })
}

fn count_partial(dir: &Path) -> usize {
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .map(|e| {
            let p = e.path();
            if p.is_dir() {
                count_partial(&p)
            } else {
                usize::from(file_name(&p).ends_with(".partial"))
            }
        })
        .sum()
}

/// Unpack a planned part (checking every file), send it in, and record it.
fn run(archive: &mut dyn Archive, plan: Plan, opts: &ImportOptions, on: &mut dyn FnMut(&Event)) -> Result<Receipt> {
    let Plan { source, part, folder, dirs, items, checked } = plan;
    let fail = |what: String| Error::Corrupt(format!("{part}: {what}"));
    let total: u64 = items.iter().map(|i| i.rec.size).sum();
    let stage = opts.staging.join(&part);
    fs::create_dir_all(&stage)?;
    if let Some(free) = util::free_space(&stage) {
        if free < total + total / 20 + (1 << 30) {
            return Err(Error::other(format!("not enough space in {} to unpack {part}", opts.staging.display())));
        }
    }
    on(&Event::Unpacking { part: part.clone(), files: items.len() as u64, bytes: total });
    let top = stage.join(&folder);
    fs::create_dir_all(&top)?;
    for d in &dirs {
        fs::create_dir_all(top.join(safe_rel(d)?))?;
    }
    let mut bundle = match &items.first().map(|i| &i.stored) {
        Some(Stored::Member(..)) => Some(File::open(&source)?),
        _ => None,
    };
    for item in &items {
        let rec = &item.rec;
        let dst = top.join(safe_rel(&rec.path)?);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        // An earlier attempt may have unpacked it already.
        if fs::metadata(&dst).is_ok_and(|md| md.len() == rec.size) && hash_reader(File::open(&dst)?)?.0 == rec.sha256 {
            continue;
        }
        let compressed: Box<dyn Read + '_> = match &item.stored {
            Stored::Member(off, len) => {
                let f = bundle.as_mut().expect("bundle opened for members");
                f.seek(SeekFrom::Start(*off))?;
                Box::new(f.take(*len))
            }
            Stored::File(p) => Box::new(File::open(p)?),
        };
        let mut dec = zstd::stream::read::Decoder::new(compressed)?;
        let partial = dst.with_file_name(format!("{}.import-partial", file_name(&dst)));
        let mut out = BufWriter::with_capacity(1 << 20, File::create(&partial)?);
        let (mut h, mut n, mut buf) = (Hasher::new(), 0u64, vec![0u8; 1 << 20]);
        loop {
            let k = dec.read(&mut buf).map_err(|e| fail(format!("{}: can't decompress: {e}", rec.path)))?;
            if k == 0 {
                break;
            }
            h.update(&buf[..k]);
            n += k as u64;
            out.write_all(&buf[..k])?;
        }
        out.into_inner().map_err(io::IntoInnerError::into_error)?.sync_all()?;
        if n != rec.size || h.finish() != rec.sha256 {
            let _ = fs::remove_file(&partial);
            return Err(fail(format!("{} doesn't match its recorded checksum", rec.path)));
        }
        let secs = rec.mtime_ns.div_euclid(1_000_000_000);
        let nanos = rec.mtime_ns.rem_euclid(1_000_000_000) as u32;
        filetime::set_file_mtime(&partial, filetime::FileTime::from_unix_time(secs, nanos))?;
        fs::rename(&partial, &dst)?;
    }

    // Send it in: the folder becomes a project (or is added to, for later parts).
    let project = opts.dest.join(&folder)?;
    on(&Event::Sending { part: part.clone(), project: project.clone() });
    let send_opts = SendOptions { mode: Mode::Move, job: format!("import-{part}"), exclude: Vec::new(), ..opts.send.clone() };
    let r: SendReport = send::send(archive, &[top.clone()], &opts.dest, &send_opts, &mut |e| {
        if let send::Event::Progress { done, total } = e {
            on(&Event::Progress { done: *done, total: *total });
        }
    })?;
    let archived = r.stored + r.deduplicated + r.identical;
    if !r.ok() || !r.skipped.is_empty() || archived != items.len() as u64 {
        let first = r
            .failed
            .first()
            .map(|(p, e)| format!("{}: {e}", p.display()))
            .or_else(|| r.skipped.first().map(|(p, d)| format!("{} is already in the archive at {d} with different contents", p.display())))
            .unwrap_or_default();
        return Err(Error::other(format!(
            "{part}: {archived} of {} files archived; the rest are still in {}. First problem: {first}",
            items.len(),
            stage.display()
        )));
    }
    let _ = fs::remove_dir_all(&stage);

    let receipt = Receipt {
        source: source.display().to_string(),
        part: part.clone(),
        folder,
        project: project.to_string(),
        files: items.len() as u64,
        bytes: total,
        checked: checked.into(),
        stored: r.stored,
        deduplicated: r.deduplicated,
        identical: r.identical,
        imported_at: now_secs(),
    };
    fs::create_dir_all(&opts.receipts)?;
    util::atomic_write(&opts.receipts.join(format!("{part}.json")), &serde_json::to_vec_pretty(&receipt)?)?;
    on(&Event::Imported { part, files: receipt.files });
    Ok(receipt)
}
