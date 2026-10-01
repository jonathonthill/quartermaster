//! Finding files by name on an ordinary file system (this computer, or a file
//! server through the helper), using the system's own index when it has one:
//! Spotlight on macOS, `plocate`/`locate` on Linux. Otherwise the folder is
//! searched file by file. Index results are checked against the disk, so files
//! deleted since the index was built don't appear.
//!
//! Matching is on names: every word must appear in the name (case-insensitive),
//! or match it as a pattern if it has `*` or `?`. The filters of
//! [`crate::words`] (type:, is:, after:, before:, sizes) apply too.

use std::fs::{self, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::send::{SYSTEM_JUNK, glob_match};
use crate::words::{self, Filter, Query};

#[derive(Clone, Debug)]
pub struct FindOptions {
    pub show_hidden: bool,
    pub limit: usize,
    pub budget: Duration,
    /// Skip the system index and look at every file.
    pub every_file: bool,
}

impl Default for FindOptions {
    fn default() -> Self {
        FindOptions { show_hidden: false, limit: 1000, budget: Duration::from_secs(20), every_file: false }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    Spotlight,
    Locate,
    Scan,
}

pub struct Found {
    pub path: PathBuf,
    pub meta: Metadata,
}

pub struct FindResult {
    pub found: Vec<Found>,
    pub method: Method,
    /// More matches exist than were returned.
    pub truncated: bool,
    /// Time ran out before the search finished.
    pub timed_out: bool,
    /// Files and folders looked at (file-by-file searches).
    pub scanned: u64,
}

struct Matcher {
    terms: Vec<String>,
    filters: Vec<Filter>,
}

impl Matcher {
    fn new(q: &Query) -> Matcher {
        Matcher { terms: words::name_terms(q), filters: q.filters.clone() }
    }

    fn name(&self, name: &str) -> bool {
        let n = name.to_lowercase();
        self.terms.iter().all(|t| if t.contains(['*', '?']) { glob_match(t, &n) } else { n.contains(t.as_str()) })
    }

    fn meta(&self, name: &str, md: &Metadata) -> bool {
        let mtime = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
        self.filters.iter().all(|f| match f {
            Filter::Ext(e) => {
                let n = name.to_lowercase();
                md.is_file() && (n.ends_with(&format!(".{e}")) || n.contains(&format!(".{e}.")))
            }
            Filter::IsDir => md.is_dir(),
            Filter::IsFile => md.is_file(),
            Filter::After(t) => mtime >= *t,
            Filter::Before(t) => mtime < *t,
            Filter::Size(op, b) => {
                let s = md.len();
                md.is_file()
                    && match *op {
                        ">" => s > *b,
                        ">=" => s >= *b,
                        "<" => s < *b,
                        _ => s <= *b,
                    }
            }
        })
    }
}

fn is_junk(name: &str) -> bool {
    SYSTEM_JUNK.iter().any(|p| glob_match(p, name))
}

/// Find names under `root` matching `query`.
pub fn find(root: &Path, query: &str, opts: &FindOptions, cancelled: &dyn Fn() -> bool) -> Result<FindResult> {
    let q = words::parse(query);
    if q.is_empty() {
        return Err(Error::InvalidPath("Type something to search for.".into()));
    }
    let m = Matcher::new(&q);
    if !opts.every_file && !m.terms.is_empty() {
        if let Some(r) = from_index(root, &m, opts, cancelled)? {
            return Ok(r);
        }
    }
    scan(root, &m, opts, cancelled)
}

/// Keep index results that exist, are under `root`, and pass the checks.
fn accept(root: &Path, paths: Vec<PathBuf>, m: &Matcher, opts: &FindOptions, recheck_names: bool, method: Method) -> FindResult {
    let mut out = FindResult { found: Vec::new(), method, truncated: false, timed_out: false, scanned: 0 };
    for p in paths {
        let Ok(rel) = p.strip_prefix(root) else { continue };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let hidden = rel.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.'));
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if (hidden && !opts.show_hidden) || is_junk(&name) || (recheck_names && !m.name(&name)) {
            continue;
        }
        let Ok(md) = fs::symlink_metadata(&p) else { continue };
        if !m.meta(&name, &md) {
            continue;
        }
        if out.found.len() >= opts.limit {
            out.truncated = true;
            break;
        }
        out.found.push(Found { path: p, meta: md });
    }
    out
}

/// Run an index query, giving up (and killing it) when cancelled or out of time.
fn run(mut cmd: Command, opts: &FindOptions, cancelled: &dyn Fn() -> bool) -> Option<Vec<PathBuf>> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let buf = reader.join().ok()?;
                if !status.success() {
                    return None;
                }
                return Some(
                    buf.split(|b| *b == 0)
                        .filter(|s| !s.is_empty())
                        .map(|s| PathBuf::from(String::from_utf8_lossy(s).into_owned()))
                        .collect(),
                );
            }
            Ok(None) if cancelled() || started.elapsed() > opts.budget => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(30)),
            Err(_) => return None,
        }
    }
}

#[cfg(target_os = "macos")]
fn from_index(root: &Path, m: &Matcher, opts: &FindOptions, cancelled: &dyn Fn() -> bool) -> Result<Option<FindResult>> {
    // Spotlight's patterns have `*` but not `?`.
    if m.terms.iter().any(|t| t.contains('?')) || !spotlight_covers(root) {
        return Ok(None);
    }
    let esc = |t: &str| t.replace('\\', "\\\\").replace('"', "\\\"");
    let query = m
        .terms
        .iter()
        .map(
            |t| if t.contains('*') { format!("kMDItemFSName == \"{}\"cd", esc(t)) } else { format!("kMDItemFSName == \"*{}*\"cd", esc(t)) },
        )
        .collect::<Vec<_>>()
        .join(" && ");
    let mut cmd = Command::new("mdfind");
    cmd.arg("-0").arg("-onlyin").arg(root).arg(query);
    if cancelled() {
        return Err(Error::other("cancelled"));
    }
    Ok(run(cmd, opts, cancelled).map(|paths| {
        // Spotlight also ignores accents, so its name matches are kept as they are.
        accept(root, paths, m, opts, false, Method::Spotlight)
    }))
}

/// Whether Spotlight really knows what's in `root`. A volume can report
/// "Indexing enabled" and still have nothing indexed (exFAT drives often do),
/// so also check that Spotlight finds a few of the folder's actual items.
#[cfg(target_os = "macos")]
fn spotlight_covers(root: &Path) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (Instant, bool)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    if let Some((when, v)) = cache.lock().unwrap().get(root) {
        if when.elapsed() < Duration::from_secs(300) {
            return *v;
        }
    }
    let covered = spotlight_indexes(root) && {
        let names: Vec<String> = fs::read_dir(root)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| !n.starts_with('.') && !is_junk(n) && !n.contains(['"', '\\', '*']))
                    .take(3)
                    .collect()
            })
            .unwrap_or_default();
        names.iter().any(|n| {
            Command::new("mdfind")
                .arg("-onlyin")
                .arg(root)
                .arg("-count")
                .arg(format!("kMDItemFSName == \"{n}\""))
                .output()
                .ok()
                .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok())
                .is_some_and(|c| c > 0)
        })
    };
    cache.lock().unwrap().insert(root.to_path_buf(), (Instant::now(), covered));
    covered
}

/// Whether Spotlight indexing is turned on for the volume holding `path`.
#[cfg(target_os = "macos")]
fn spotlight_indexes(path: &Path) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let Some(volume) = mount_point(path) else { return false };
    let cache = CACHE.get_or_init(Mutex::default);
    if let Some(v) = cache.lock().unwrap().get(&volume) {
        return *v;
    }
    let on = Command::new("mdutil")
        .arg("-s")
        .arg(&volume)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("Indexing enabled"))
        .unwrap_or(false);
    cache.lock().unwrap().insert(volume, on);
    on
}

#[cfg(target_os = "macos")]
fn mount_point(path: &Path) -> Option<String> {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let name = unsafe { CStr::from_ptr(st.f_mntonname.as_ptr()) };
    Some(name.to_string_lossy().into_owned())
}

#[cfg(target_os = "linux")]
fn from_index(root: &Path, m: &Matcher, opts: &FindOptions, cancelled: &dyn Fn() -> bool) -> Result<Option<FindResult>> {
    locate_index(root, m, opts, cancelled)
}

/// Linux: the `plocate` or `locate` database, if it covers `root`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn locate_index(root: &Path, m: &Matcher, opts: &FindOptions, cancelled: &dyn Fn() -> bool) -> Result<Option<FindResult>> {
    // plocate matches every pattern; mlocate and GNU locate need --all for that.
    let (bin, needs_all) = match ["plocate", "locate"].iter().find_map(|b| {
        let out = Command::new(b).arg("--version").output().ok().filter(|o| o.status.success())?;
        let text = String::from_utf8_lossy(&out.stdout).to_lowercase() + &String::from_utf8_lossy(&out.stderr).to_lowercase();
        Some((b.to_string(), !text.contains("plocate")))
    }) {
        Some(x) => x,
        None => return Ok(None),
    };
    // Is this folder in the index at all? (Network mounts often aren't.)
    let prefix = format!("{}/", root.display().to_string().trim_end_matches('/'));
    let covered = Command::new(&bin)
        .args(["-c", "-l", "1"])
        .arg(&prefix)
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok())
        .is_some_and(|n| n > 0);
    if !covered {
        return Ok(None);
    }
    let mut cmd = Command::new(&bin);
    cmd.args(["-i", "-0", "-l", "200000"]);
    if needs_all {
        cmd.arg("-A");
    }
    cmd.arg(&prefix);
    for t in &m.terms {
        cmd.arg(if t.contains(['*', '?']) { format!("*/{t}") } else { t.clone() });
    }
    if cancelled() {
        return Err(Error::other("cancelled"));
    }
    Ok(run(cmd, opts, cancelled).map(|paths| accept(root, paths, m, opts, true, Method::Locate)))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn from_index(_: &Path, _: &Matcher, _: &FindOptions, _: &dyn Fn() -> bool) -> Result<Option<FindResult>> {
    Ok(None)
}

/// Look at every file and folder under `root`.
fn scan(root: &Path, m: &Matcher, opts: &FindOptions, cancelled: &dyn Fn() -> bool) -> Result<FindResult> {
    let started = Instant::now();
    let mut out = FindResult { found: Vec::new(), method: Method::Scan, truncated: false, timed_out: false, scanned: 0 };
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            out.scanned += 1;
            if out.scanned % 256 == 0 {
                if cancelled() {
                    return Err(Error::other("cancelled"));
                }
                if started.elapsed() > opts.budget {
                    out.timed_out = true;
                    return Ok(out);
                }
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if is_junk(&name) || (!opts.show_hidden && name.starts_with('.')) {
                continue;
            }
            let Ok(md) = fs::symlink_metadata(e.path()) else { continue };
            if md.is_dir() {
                stack.push(e.path());
            }
            if (m.terms.is_empty() || m.name(&name)) && m.meta(&name, &md) {
                if out.found.len() >= opts.limit {
                    out.truncated = true;
                    return Ok(out);
                }
                out.found.push(Found { path: e.path(), meta: md });
            }
        }
    }
    Ok(out)
}

/// Seconds since the epoch for a file's modified time (0 if unknown).
pub fn mtime_secs(md: &Metadata) -> i64 {
    md.modified().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (p, size) in [
            ("raw/run1_Sample.fastq.gz", 5000),
            ("raw/run2.fastq.gz", 10),
            ("analysis/tables/sample_summary.csv", 10),
            ("notes.txt", 10),
            (".hidden/sample.txt", 10),
        ] {
            let f = d.path().join(p);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(f, vec![0u8; size]).unwrap();
        }
        fs::create_dir_all(d.path().join("sample_plots")).unwrap();
        fs::write(d.path().join("raw/._run1_Sample.fastq.gz"), b"junk").unwrap();
        d
    }

    fn names(r: &FindResult) -> Vec<String> {
        let mut v: Vec<String> = r.found.iter().map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    fn every(show_hidden: bool) -> FindOptions {
        FindOptions { show_hidden, every_file: true, ..FindOptions::default() }
    }

    #[test]
    fn file_by_file_search() {
        let d = tree();
        let never = || false;
        let r = find(d.path(), "sample", &every(false), &never).unwrap();
        assert_eq!(r.method, Method::Scan);
        assert_eq!(names(&r), ["run1_Sample.fastq.gz", "sample_plots", "sample_summary.csv"]);
        assert_eq!(find(d.path(), "sample", &every(true), &never).unwrap().found.len(), 4);
        assert_eq!(names(&find(d.path(), "run?.fastq*", &every(false), &never).unwrap()), ["run2.fastq.gz"]);
        assert_eq!(names(&find(d.path(), "run fastq", &every(false), &never).unwrap()), ["run1_Sample.fastq.gz", "run2.fastq.gz"]);
        assert_eq!(names(&find(d.path(), "sample is:folder", &every(false), &never).unwrap()), ["sample_plots"]);
        assert_eq!(names(&find(d.path(), "type:fastq >1KB", &every(false), &never).unwrap()), ["run1_Sample.fastq.gz"]);
        assert!(find(d.path(), "  ", &every(false), &never).is_err());
    }

    #[test]
    fn limits_and_cancellation() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..600 {
            fs::write(d.path().join(format!("file{i}.dat")), b"").unwrap();
        }
        let opts = FindOptions { limit: 50, ..every(false) };
        let r = find(d.path(), "file", &opts, &|| false).unwrap();
        assert_eq!(r.found.len(), 50);
        assert!(r.truncated);
        assert!(find(d.path(), "file", &every(false), &|| true).is_err());
    }

    #[test]
    fn index_results_are_checked_against_the_disk() {
        let d = tree();
        let m = Matcher::new(&words::parse("sample"));
        let paths = vec![
            d.path().join("raw/run1_Sample.fastq.gz"),
            d.path().join("raw/deleted_sample.txt"),
            d.path().join(".hidden/sample.txt"),
            PathBuf::from("/elsewhere/sample.txt"),
            d.path().join("notes.txt"),
        ];
        let r = accept(d.path(), paths, &m, &every(false), true, Method::Locate);
        assert_eq!(names(&r), ["run1_Sample.fastq.gz"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn spotlight_is_used_when_the_volume_is_indexed() {
        // Can't wait for Spotlight to index a new temp folder; just check that an
        // indexed volume is detected and a query runs without error.
        let home = std::env::var("HOME").unwrap();
        if !spotlight_covers(Path::new(&home)) {
            eprintln!("skipping: home folder isn't indexed");
            return;
        }
        let r = find(Path::new(&home), "zzq-no-such-file-name-xy", &FindOptions::default(), &|| false).unwrap();
        assert_eq!(r.method, Method::Spotlight);
        assert!(r.found.is_empty());
    }
}
