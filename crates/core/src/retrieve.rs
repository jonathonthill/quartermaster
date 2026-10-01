//! The receiver: copies files or folders out of an archive onto local disk.
//!
//! Each file is written to a hidden `.partial` file beside its destination,
//! checked against the SHA-256 recorded when it was archived, fsynced, given
//! back its modification time and permissions, and only then renamed into
//! place. A failed check never leaves a wrong file under the real name.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::api::Archive;
use crate::catalog::{Entry, NodeKind};
use crate::error::{Error, Result};
use crate::hash::Hasher;
use crate::util;
use crate::vpath::VPath;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LocalConflict {
    /// Leave the existing local file alone.
    #[default]
    Skip,
    /// Overwrite the existing local file (only after the new copy is verified).
    Replace,
    /// Save as "name (2).ext".
    KeepBoth,
}

#[derive(Clone, Debug)]
pub struct RetrieveOptions {
    pub policy: LocalConflict,
    pub retries: u32,
    /// Set to stop. The file in flight is discarded; finished files stay.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for RetrieveOptions {
    fn default() -> Self {
        RetrieveOptions { policy: LocalConflict::Skip, retries: 2, cancel: None }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Planned { files: u64, bytes: u64 },
    Receiving { path: PathBuf },
    Progress { done: u64, total: u64 },
    Written { path: PathBuf },
    Skipped { path: PathBuf },
    Renamed { from: String, to: String },
    Failed { path: PathBuf, error: String },
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RetrieveReport {
    pub files: u64,
    pub bytes: u64,
    pub written: u64,
    pub skipped: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
    /// Names changed to be valid on this system (Windows).
    pub renamed: Vec<(String, String)>,
    pub cancelled: bool,
}

impl RetrieveReport {
    pub fn ok(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Make a name valid on Windows: replace reserved characters, strip trailing
/// dots and spaces, and prefix reserved device names.
pub fn windows_safe_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| if matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*') || (c as u32) < 32 { '_' } else { c })
        .collect();
    while s.ends_with('.') || s.ends_with(' ') {
        s.pop();
    }
    if s.is_empty() {
        s.push('_');
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        s.insert(0, '_');
    }
    s
}

fn local_name(name: &str, renamed: &mut Vec<(String, String)>) -> String {
    if cfg!(windows) {
        let safe = windows_safe_name(name);
        if safe != name {
            renamed.push((name.to_string(), safe.clone()));
        }
        safe
    } else {
        name.to_string()
    }
}

struct Job {
    rel: Vec<String>,
    entry: Entry,
    src: VPath,
}

/// Copy `src` (a file, folder, or link in the archive) into the local folder `dest_dir`.
pub fn retrieve(
    archive: &mut dyn Archive,
    src: &VPath,
    dest_dir: &Path,
    opts: &RetrieveOptions,
    on: &mut dyn FnMut(&Event),
) -> Result<RetrieveReport> {
    let top = archive.stat(src)?.ok_or_else(|| Error::NotFound(src.to_string()))?;
    let top_name = src.name().ok_or_else(|| Error::InvalidPath("choose a file or folder, not the whole archive".into()))?;
    let mut jobs = vec![Job { rel: vec![top_name.to_string()], entry: top.clone(), src: src.clone() }];
    if top.kind == NodeKind::Dir {
        for (rel, e) in archive.walk(src)? {
            let mut parts = vec![top_name.to_string()];
            parts.extend(rel.split('/').map(str::to_string));
            jobs.push(Job { rel: parts, src: src.join_rel(&rel)?, entry: e });
        }
    }
    let files: Vec<&Job> = jobs.iter().filter(|j| j.entry.kind == NodeKind::File).collect();
    let mut report = RetrieveReport { files: files.len() as u64, bytes: files.iter().map(|j| j.entry.size).sum(), ..Default::default() };
    on(&Event::Planned { files: report.files, bytes: report.bytes });

    let local_path = |rel: &[String], renamed: &mut Vec<(String, String)>| -> PathBuf {
        let mut p = dest_dir.to_path_buf();
        for c in rel {
            p.push(local_name(c, renamed));
        }
        p
    };

    let mut done = 0u64;
    let mut dirs_to_stamp = Vec::new();
    for j in jobs.iter().filter(|j| j.entry.kind == NodeKind::Dir) {
        let p = local_path(&j.rel, &mut report.renamed);
        if let Err(e) = fs::create_dir_all(&p) {
            report.failed.push((p.clone(), e.to_string()));
            on(&Event::Failed { path: p, error: e.to_string() });
        } else {
            dirs_to_stamp.push((p, j.entry.mtime_ns));
        }
    }
    for (from, to) in &report.renamed {
        on(&Event::Renamed { from: from.clone(), to: to.clone() });
    }

    // Decide each file's destination (and skip existing ones) up front.
    let mut todo: Vec<(&Job, PathBuf)> = Vec::new();
    for j in &files {
        let mut target = local_path(&j.rel, &mut report.renamed);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        if fs::symlink_metadata(&target).is_ok() {
            match opts.policy {
                LocalConflict::Skip => {
                    done += j.entry.size;
                    report.skipped.push(target.clone());
                    on(&Event::Skipped { path: target });
                    on(&Event::Progress { done, total: report.bytes });
                    continue;
                }
                LocalConflict::Replace => {}
                LocalConflict::KeepBoth => target = free_local_name(&target),
            }
        }
        todo.push((j, target));
    }

    // Fetch in batches, so a remote archive streams many files per round trip.
    let is_cancelled = || opts.cancel.as_ref().is_some_and(|f| f.load(Ordering::Relaxed));
    let mut retry: Vec<(&Job, PathBuf)> = Vec::new();
    let mut start = 0;
    while start < todo.len() && !is_cancelled() {
        let mut end = start;
        let mut bytes = 0u64;
        while end < todo.len() && end - start < 2000 && (end == start || bytes + todo[end].0.entry.size <= 512 << 20) {
            bytes += todo[end].0.entry.size;
            end += 1;
        }
        let batch = &todo[start..end];
        let paths: Vec<VPath> = batch.iter().map(|(j, _)| j.src.clone()).collect();
        archive.read_files(&paths, &mut |i, res| {
            if is_cancelled() {
                return Err(Error::other("cancelled"));
            }
            let (j, target) = &batch[i];
            on(&Event::Receiving { path: target.clone() });
            let (base, total) = (done, report.bytes);
            let mut progress = |n: u64| on(&Event::Progress { done: base + n, total });
            let res = res.and_then(|(entry, reader)| {
                let mut r = crate::send::CancelRead { inner: reader, flag: opts.cancel.clone() };
                write_verified(&entry, &mut r, target, &mut progress)
            });
            if is_cancelled() {
                return Err(Error::other("cancelled"));
            }
            match res {
                Ok(()) => {
                    report.written += 1;
                    on(&Event::Written { path: target.clone() });
                    done += j.entry.size;
                    on(&Event::Progress { done, total: report.bytes });
                }
                Err(Error::Verify(_)) if opts.retries > 0 => retry.push((j, target.clone())),
                Err(e) => {
                    report.failed.push((target.clone(), e.to_string()));
                    on(&Event::Failed { path: target.clone(), error: e.to_string() });
                    done += j.entry.size;
                    on(&Event::Progress { done, total: report.bytes });
                }
            }
            Ok(())
        })?;
        start = end;
    }

    // Files that failed verification get fetched again, one at a time.
    for (j, target) in retry.into_iter().take_while(|_| !is_cancelled()) {
        let mut attempt = 1;
        loop {
            let res = archive.read_file(&j.src).and_then(|(entry, mut reader)| write_verified(&entry, &mut reader, &target, &mut |_| {}));
            match res {
                Ok(()) => {
                    report.written += 1;
                    on(&Event::Written { path: target.clone() });
                    break;
                }
                Err(Error::Verify(_)) if attempt < opts.retries => attempt += 1,
                Err(e) => {
                    report.failed.push((target.clone(), e.to_string()));
                    on(&Event::Failed { path: target.clone(), error: e.to_string() });
                    break;
                }
            }
        }
        done += j.entry.size;
        on(&Event::Progress { done, total: report.bytes });
    }

    for j in jobs.iter().filter(|j| j.entry.kind == NodeKind::Symlink) {
        let target = local_path(&j.rel, &mut report.renamed);
        let link_to = j.entry.target.clone().unwrap_or_default();
        if fs::symlink_metadata(&target).is_ok() {
            report.skipped.push(target.clone());
            on(&Event::Skipped { path: target });
            continue;
        }
        if let Err(e) = make_symlink(&link_to, &target) {
            report.failed.push((target.clone(), e.to_string()));
            on(&Event::Failed { path: target, error: e.to_string() });
        }
    }

    report.cancelled = is_cancelled();

    // Make the renames durable: one flush per folder rather than per file.
    let mut synced = std::collections::HashSet::new();
    for j in &files {
        let dir = local_path(&j.rel, &mut Vec::new()).parent().map(Path::to_path_buf);
        if let Some(d) = dir {
            if synced.insert(d.clone()) {
                let _ = util::sync_dir(&d);
            }
        }
    }

    // Folder times last, since writing files inside them changes them.
    for (p, mtime_ns) in dirs_to_stamp.iter().rev() {
        if *mtime_ns != 0 {
            let _ = filetime::set_file_mtime(p, filetime_from_ns(*mtime_ns));
        }
    }
    Ok(report)
}

/// Write one file beside `target`, check it against the archived SHA-256,
/// restore its time and permissions, and only then rename it into place.
fn write_verified(entry: &Entry, reader: &mut dyn Read, target: &Path, progress: &mut dyn FnMut(u64)) -> Result<()> {
    let mut reader = crate::send::Counting::new(reader, progress);
    let dir = target.parent().unwrap_or(Path::new("."));
    let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.archive-partial"));
    let res = (|| -> Result<()> {
        let mut out = File::create(&tmp)?;
        let mut h = Hasher::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut n = 0u64;
        loop {
            let k = reader.read(&mut buf).map_err(|e| Error::Verify(format!("read failed: {e}")))?;
            if k == 0 {
                break;
            }
            h.update(&buf[..k]);
            out.write_all(&buf[..k])?;
            n += k as u64;
        }
        util::fsync_light(&out)?;
        let got = h.finish();
        if Some(got) != entry.sha256 || n != entry.size {
            return Err(Error::Verify(format!("{}: checksum mismatch after download", entry.name)));
        }
        drop(out);
        #[cfg(unix)]
        if entry.mode != 0 {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(entry.mode & 0o7777))?;
        }
        filetime::set_file_mtime(&tmp, filetime_from_ns(entry.mtime_ns))?;
        if cfg!(windows) && target.exists() {
            fs::remove_file(target)?;
        }
        fs::rename(&tmp, target)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

fn free_local_name(target: &Path) -> PathBuf {
    let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let mut n = 2;
    loop {
        let cand = target.with_file_name(util::numbered_name(name, n));
        if fs::symlink_metadata(&cand).is_err() {
            return cand;
        }
        n += 1;
    }
}

fn filetime_from_ns(ns: i64) -> filetime::FileTime {
    filetime::FileTime::from_unix_time(ns.div_euclid(1_000_000_000), ns.rem_euclid(1_000_000_000) as u32)
}

#[cfg(unix)]
fn make_symlink(target: &str, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn make_symlink(target: &str, link: &Path) -> std::io::Result<()> {
    // Windows needs a privilege or Developer Mode for symlinks; try, and report failure.
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_names() {
        assert_eq!(windows_safe_name("a:b?.txt"), "a_b_.txt");
        assert_eq!(windows_safe_name("CON"), "_CON");
        assert_eq!(windows_safe_name("com1.log"), "_com1.log");
        assert_eq!(windows_safe_name("console.txt"), "console.txt");
        assert_eq!(windows_safe_name("trailing. "), "trailing");
    }
}
