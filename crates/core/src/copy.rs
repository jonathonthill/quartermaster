//! Plain copies and moves between ordinary file systems: this computer or a
//! file server (`archive-helper files`), in any combination. Nothing here
//! touches the archive format.
//!
//! Each file is written to a hidden `.qm-part` file beside its destination,
//! checked against the SHA-256 of what the source sent, given back its
//! modification time and permissions, and only then renamed into place, so a
//! failed or interrupted copy never leaves a wrong file under the real name.
//! A part file that survives an interruption is continued from where it
//! stopped. Moving deletes originals only once *every* file has been copied
//! and verified, and then only those that haven't changed since they were
//! scanned; if anything fails or the copy is stopped, nothing is deleted.
//! Until then a Move is a Copy, so stopping it can undo it completely.
//!
//! The engine talks to both sides through [`Endpoint`], so the same code
//! copies local to server, server to local, and server to server (with the
//! bytes passing through this computer).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::hash::{Digest, Hasher};
use crate::proto::CHUNK;
use crate::retrieve::LocalConflict;
use crate::send::{Mode, SYSTEM_JUNK, glob_match};
use crate::util;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    File,
    Dir,
    Link,
}

/// What an endpoint knows about one path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStat {
    pub kind: FileKind,
    pub size: u64,
    pub mtime_ns: i64,
    /// Status-change time and inode, where the system has them (0 otherwise);
    /// with size and mtime they tell whether a file changed since a scan.
    #[serde(default)]
    pub ctime_ns: i64,
    #[serde(default)]
    pub inode: u64,
    pub mode: u32,
}

impl FileStat {
    pub fn same_file_as(&self, other: &FileStat) -> bool {
        self.kind == other.kind
            && self.size == other.size
            && self.mtime_ns == other.mtime_ns
            && self.ctime_ns == other.ctime_ns
            && self.inode == other.inode
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalkItem {
    /// Path from the chosen folder's parent, `/`-separated: the chosen item's
    /// own name first, then everything under it.
    pub rel: String,
    pub path: String,
    pub stat: FileStat,
}

/// A scan of a folder tree. Folders that couldn't be read are listed in
/// `problems` (path, reason) rather than ending the scan.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Walk {
    pub items: Vec<WalkItem>,
    pub problems: Vec<(String, String)>,
}

// ---------------------------------------------------------------- local disk

pub fn stat_path(p: &Path, follow: bool) -> std::io::Result<FileStat> {
    let md = if follow { fs::metadata(p)? } else { fs::symlink_metadata(p)? };
    let kind = if md.file_type().is_symlink() {
        FileKind::Link
    } else if md.is_dir() {
        FileKind::Dir
    } else {
        FileKind::File
    };
    #[cfg(unix)]
    let (ctime_ns, inode) = {
        use std::os::unix::fs::MetadataExt;
        (md.ctime() * 1_000_000_000 + md.ctime_nsec(), md.ino())
    };
    #[cfg(not(unix))]
    let (ctime_ns, inode) = (0, 0);
    Ok(FileStat {
        kind,
        size: if kind == FileKind::File { md.len() } else { 0 },
        mtime_ns: crate::send::mtime_ns(&md),
        ctime_ns,
        inode,
        mode: crate::send::mode_of(&md),
    })
}

/// Scan `root` and everything under it. Links are listed but not followed
/// (the chosen item itself may be a link to a folder).
pub fn walk_local(root: &Path) -> Result<Walk> {
    let top = stat_path(root, true).map_err(|e| Error::other(format!("Can't read {}: {e}", root.display())))?;
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| Error::InvalidPath("choose a file or folder, not the whole disk".into()))?;
    let mut walk = Walk::default();
    let is_dir = top.kind == FileKind::Dir;
    walk.items.push(WalkItem { rel: name.clone(), path: root.display().to_string(), stat: top });
    if is_dir {
        walk_dir(root, &name, &mut walk);
    }
    Ok(walk)
}

fn walk_dir(dir: &Path, rel: &str, walk: &mut Walk) {
    let mut names: Vec<_> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.file_name()).collect(),
        Err(e) => {
            walk.problems.push((dir.display().to_string(), format!("Can't open the folder: {e}")));
            return;
        }
    };
    names.sort();
    for n in names {
        let p = dir.join(&n);
        let r = format!("{rel}/{}", n.to_string_lossy());
        match stat_path(&p, false) {
            Ok(stat) => {
                let is_dir = stat.kind == FileKind::Dir;
                walk.items.push(WalkItem { rel: r.clone(), path: p.display().to_string(), stat });
                if is_dir {
                    walk_dir(&p, &r, walk);
                }
            }
            Err(e) => walk.problems.push((p.display().to_string(), e.to_string())),
        }
    }
}

/// The hidden file a copy is written to before it is renamed into place.
pub fn part_path(target: &Path) -> PathBuf {
    let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    target.with_file_name(format!(".{name}.qm-part"))
}

fn filetime_from_ns(ns: i64) -> filetime::FileTime {
    filetime::FileTime::from_unix_time(ns.div_euclid(1_000_000_000), ns.rem_euclid(1_000_000_000) as u32)
}

/// Read `path` from `offset`, handing each chunk to `sink`. Returns the SHA-256
/// of the *whole* file: the part before `offset` is read and hashed but not sent.
pub fn read_stream(path: &Path, offset: u64, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<Digest> {
    let mut f = File::open(path).map_err(|e| Error::other(format!("Can't read {}: {e}", path.display())))?;
    let mut h = Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    let mut left = offset;
    while left > 0 {
        let k = f.read(&mut buf[..left.min(CHUNK as u64) as usize])?;
        if k == 0 {
            return Err(Error::other(format!("{} is shorter than the part already copied", path.display())));
        }
        h.update(&buf[..k]);
        left -= k as u64;
    }
    loop {
        let k = f.read(&mut buf)?;
        if k == 0 {
            return Ok(h.finish());
        }
        h.update(&buf[..k]);
        sink(&buf[..k])?;
    }
}

/// Writes one file to a part file and, once the caller says the stream is
/// complete and gives the source's checksum, verifies and renames it.
pub struct PartWriter {
    file: File,
    part: PathBuf,
    target: PathBuf,
    hasher: Hasher,
    written: u64,
    size: u64,
    mtime_ns: i64,
    mode: u32,
}

impl PartWriter {
    /// Start (or with `offset > 0` continue) writing `target`. The folder must exist.
    pub fn begin(target: &Path, size: u64, mtime_ns: i64, mode: u32, offset: u64) -> Result<PartWriter> {
        let part = part_path(target);
        let mut hasher = Hasher::new();
        let file = if offset > 0 {
            let mut f =
                OpenOptions::new().read(true).write(true).open(&part).map_err(|_| Error::NotFound("the partial copy is gone".into()))?;
            if f.metadata()?.len() < offset {
                return Err(Error::other("the partial copy is shorter than expected"));
            }
            let mut buf = vec![0u8; CHUNK];
            let mut left = offset;
            while left > 0 {
                let k = f.read(&mut buf[..left.min(CHUNK as u64) as usize])?;
                if k == 0 {
                    return Err(Error::other("the partial copy is shorter than expected"));
                }
                hasher.update(&buf[..k]);
                left -= k as u64;
            }
            f.set_len(offset)?;
            f.seek(SeekFrom::Start(offset))?;
            f
        } else {
            File::create(&part).map_err(|e| Error::other(format!("Can't write {}: {e}", target.display())))?
        };
        Ok(PartWriter { file, part, target: target.to_path_buf(), hasher, written: offset, size, mtime_ns, mode })
    }

    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.hasher.update(data);
        self.file.write_all(data).map_err(|e| Error::other(format!("Can't write {}: {e}", self.target.display())))?;
        self.written += data.len() as u64;
        Ok(())
    }

    /// Check the file against `digest` and put it in place. A mismatch deletes
    /// the part file and fails; nothing appears under the real name.
    pub fn finish(self, digest: Digest) -> Result<()> {
        let PartWriter { file, part, target, hasher, written, size, mtime_ns, mode } = self;
        let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
        if written != size || hasher.finish() != digest {
            drop(file);
            let _ = fs::remove_file(&part);
            return Err(Error::Verify(format!("{name}: checksum mismatch after copying")));
        }
        util::fsync_light(&file)?;
        drop(file);
        #[cfg(unix)]
        if mode != 0 {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&part, fs::Permissions::from_mode(mode & 0o7777))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        filetime::set_file_mtime(&part, filetime_from_ns(mtime_ns))?;
        if cfg!(windows) && target.exists() {
            fs::remove_file(&target)?;
        }
        fs::rename(&part, &target)?;
        Ok(())
    }
}

/// Remove a file, or an empty folder. Never removes anything recursively.
pub fn remove_path(p: &Path) -> std::io::Result<()> {
    match fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(_) if fs::symlink_metadata(p).map(|m| m.is_dir()).unwrap_or(false) => fs::remove_dir(p),
        Err(e) => Err(e),
    }
}

// ----------------------------------------------------------------- endpoints

/// One side of a copy. Writes are a sequence: `begin_write`, any number of
/// `write`s, then `finish_write` (or `abort_write`, which keeps the part file
/// so the copy can continue later).
pub trait Endpoint {
    /// Identifies the machine, to tell whether both sides are the same one.
    fn machine(&self) -> String;
    fn join(&self, dir: &str, name: &str) -> String;
    /// A name that is valid on this side (Windows has reserved characters).
    fn safe_name(&self, name: &str) -> String {
        name.to_string()
    }
    /// Whether what is written here is checked against the source's checksum. (An SFTP server can
    /// only report sizes, unless it reads uploads back.)
    fn verifies_writes(&self) -> bool {
        true
    }
    /// Whether what is read from here is checksummed by the side that stores it.
    fn verifies_reads(&self) -> bool {
        true
    }
    fn walk(&mut self, root: &str) -> Result<Walk>;
    fn stat(&mut self, path: &str) -> Result<Option<FileStat>>;
    fn mkdirs(&mut self, dirs: &[String]) -> Result<()>;
    /// Send `path` from `offset` to `sink`; returns the SHA-256 of the whole file.
    fn read(&mut self, path: &str, offset: u64, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<Digest>;
    fn begin_write(&mut self, path: &str, size: u64, mtime_ns: i64, mode: u32, offset: u64) -> Result<()>;
    fn write(&mut self, data: &[u8]) -> Result<()>;
    fn finish_write(&mut self, digest: Digest) -> Result<()>;
    fn abort_write(&mut self);
    /// Remove a file or an empty folder.
    fn remove(&mut self, path: &str) -> Result<()>;
}

/// This computer.
#[derive(Default)]
pub struct LocalEnd {
    writer: Option<PartWriter>,
}

impl LocalEnd {
    pub fn new() -> LocalEnd {
        LocalEnd::default()
    }
}

impl Endpoint for LocalEnd {
    fn machine(&self) -> String {
        "this computer".into()
    }
    fn join(&self, dir: &str, name: &str) -> String {
        Path::new(dir).join(name).display().to_string()
    }
    fn safe_name(&self, name: &str) -> String {
        if cfg!(windows) { crate::retrieve::windows_safe_name(name) } else { name.to_string() }
    }
    fn walk(&mut self, root: &str) -> Result<Walk> {
        walk_local(Path::new(root))
    }
    fn stat(&mut self, path: &str) -> Result<Option<FileStat>> {
        match stat_path(Path::new(path), false) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn mkdirs(&mut self, dirs: &[String]) -> Result<()> {
        for d in dirs {
            fs::create_dir_all(d).map_err(|e| Error::other(format!("Can't create the folder {d}: {e}")))?;
        }
        Ok(())
    }
    fn read(&mut self, path: &str, offset: u64, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<Digest> {
        read_stream(Path::new(path), offset, sink)
    }
    fn begin_write(&mut self, path: &str, size: u64, mtime_ns: i64, mode: u32, offset: u64) -> Result<()> {
        self.writer = Some(PartWriter::begin(Path::new(path), size, mtime_ns, mode, offset)?);
        Ok(())
    }
    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.writer.as_mut().ok_or_else(|| Error::other("no file is being written"))?.write(data)
    }
    fn finish_write(&mut self, digest: Digest) -> Result<()> {
        self.writer.take().ok_or_else(|| Error::other("no file is being written"))?.finish(digest)
    }
    fn abort_write(&mut self) {
        self.writer = None;
    }
    fn remove(&mut self, path: &str) -> Result<()> {
        remove_path(Path::new(path)).map_err(|e| Error::other(format!("Can't remove {path}: {e}")))
    }
}

/// A file server (`archive-helper files`), reached over its protocol.
impl Endpoint for crate::remote::Remote {
    fn machine(&self) -> String {
        self.hello().host.clone()
    }
    fn join(&self, dir: &str, name: &str) -> String {
        if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
    }
    fn walk(&mut self, root: &str) -> Result<Walk> {
        self.fs_walk(root)
    }
    fn stat(&mut self, path: &str) -> Result<Option<FileStat>> {
        self.fs_stat(path)
    }
    fn mkdirs(&mut self, dirs: &[String]) -> Result<()> {
        self.fs_mkdirs(dirs)
    }
    fn read(&mut self, path: &str, offset: u64, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<Digest> {
        self.fs_read(path, offset, sink)
    }
    fn begin_write(&mut self, path: &str, size: u64, mtime_ns: i64, mode: u32, offset: u64) -> Result<()> {
        self.fs_write_begin(path, size, mtime_ns, mode, offset)
    }
    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.fs_write_data(data)
    }
    fn finish_write(&mut self, digest: Digest) -> Result<()> {
        self.fs_write_end(digest)
    }
    fn abort_write(&mut self) {
        let _ = self.fs_write_abort();
    }
    fn remove(&mut self, path: &str) -> Result<()> {
        self.fs_remove(path)
    }
}

// -------------------------------------------------------------------- engine

#[derive(Clone, Debug)]
pub struct CopyOptions {
    pub mode: Mode,
    /// What to do when something is already at the destination. Keep both puts a chosen
    /// item beside the one that's there ("X (2)"); otherwise a folder is merged into, and a
    /// different file is skipped or replaced. (A file with the same size and date is left alone.)
    pub policy: LocalConflict,
    /// Items an earlier attempt of this same copy created at the destination. A continued
    /// Keep both fills "X (2)" again instead of starting "X (3)".
    pub reuse: Vec<String>,
    pub retries: u32,
    /// Set to stop. The file in flight keeps its part file; finished files stay.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for CopyOptions {
    fn default() -> Self {
        CopyOptions { mode: Mode::Copy, policy: LocalConflict::Skip, reuse: Vec::new(), retries: 2, cancel: None }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Scanned {
        files: u64,
        bytes: u64,
    },
    /// Starting a file: where it comes from and where it is going (its part
    /// file is `part_string(to)`).
    Copying {
        path: String,
        to: String,
    },
    /// Move: reading a file that is already at the destination, to make sure it is the same one.
    Checking {
        path: String,
    },
    Progress {
        done: u64,
        total: u64,
    },
    Copied {
        path: String,
    },
    /// A file or folder this run created where nothing was before (for Abandon).
    Created {
        path: String,
        is_dir: bool,
    },
    Skipped {
        path: String,
        why: String,
    },
    /// Move: the original was removed, after everything was copied and verified.
    Removed {
        path: String,
    },
    /// Move: the original was left in place.
    Kept {
        path: String,
        why: String,
    },
    Failed {
        path: String,
        error: String,
    },
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CopyReport {
    pub files: u64,
    pub bytes: u64,
    pub copied: u64,
    pub skipped: Vec<(String, String)>,
    pub failed: Vec<(String, String)>,
    pub removed: u64,
    /// Move: nothing was deleted from the source, since not everything was copied.
    pub originals_kept: bool,
    pub kept: Vec<(String, String)>,
    pub renamed: Vec<(String, String)>,
    /// Keep both: items that went beside one already there (the one there, the new one).
    pub kept_both: Vec<(String, String)>,
    /// Files were checked by size and date only (an SFTP server was involved), not by checksum.
    pub size_only: bool,
    pub cancelled: bool,
}

impl CopyReport {
    pub fn ok(&self) -> bool {
        self.failed.is_empty() && !self.cancelled
    }
}

fn is_junk(name: &str) -> bool {
    SYSTEM_JUNK.iter().any(|p| glob_match(p, name))
}

fn cancelled_error() -> Error {
    Error::other("cancelled")
}

#[derive(Clone)]
struct Planned {
    src: String,
    dst: String,
    stat: FileStat,
    /// Something was already at `dst`, which this copy replaces.
    existed: bool,
}

/// Copy or move `sources` (paths on `src`) into the folder `dest_dir` on `dst`.
///
/// Returns `Err` only when the copy can't go on (a lost connection, a missing
/// destination folder); problems with single files are in the report.
pub fn copy(
    src: &mut dyn Endpoint,
    dst: &mut dyn Endpoint,
    sources: &[String],
    dest_dir: &str,
    opts: &CopyOptions,
    on: &mut dyn FnMut(&Event),
) -> Result<CopyReport> {
    let is_cancelled = || opts.cancel.as_ref().is_some_and(|f| f.load(Ordering::Relaxed));
    let mut report = CopyReport { size_only: !(src.verifies_reads() && dst.verifies_writes()), ..CopyReport::default() };

    match dst.stat(dest_dir)? {
        Some(s) if s.kind == FileKind::Dir => {}
        Some(_) => return Err(Error::NotADirectory(dest_dir.to_string())),
        None => return Err(Error::NotFound(format!("the destination folder {dest_dir} doesn't exist"))),
    }

    // Scan everything first, so the total is known up front.
    let mut items: Vec<WalkItem> = Vec::new();
    for s in sources {
        // Gone already (a Move that continues after part of it was done): nothing to copy.
        if src.stat(s)?.is_none() {
            let why = "no longer there".to_string();
            report.skipped.push((s.clone(), why.clone()));
            on(&Event::Skipped { path: s.clone(), why });
            continue;
        }
        let walk = src.walk(s)?;
        for (path, why) in walk.problems {
            report.failed.push((path.clone(), why.clone()));
            on(&Event::Failed { path, error: why });
        }
        items.extend(walk.items.into_iter().filter(|i| !i.rel.split('/').any(is_junk)));
    }
    let same_machine = src.machine() == dst.machine();
    if same_machine {
        for i in items.iter().filter(|i| i.stat.kind == FileKind::Dir && !i.rel.contains('/')) {
            let inside = dest_dir == i.path || dest_dir.strip_prefix(&i.path).is_some_and(|r| r.starts_with('/') || r.starts_with('\\'));
            if inside {
                return Err(Error::other(format!("Can't copy the folder “{}” into itself.", i.rel)));
            }
        }
    }
    for i in items.iter().filter(|i| i.stat.kind == FileKind::Link) {
        report.skipped.push((i.path.clone(), "shortcut (symbolic link)".into()));
        on(&Event::Skipped { path: i.path.clone(), why: "shortcut (symbolic link)".into() });
    }
    report.files = items.iter().filter(|i| i.stat.kind == FileKind::File).count() as u64;
    report.bytes = items.iter().filter(|i| i.stat.kind == FileKind::File).map(|i| i.stat.size).sum();
    on(&Event::Scanned { files: report.files, bytes: report.bytes });

    // Where each chosen item lands. With Keep both, an item whose name is taken goes beside
    // it under a new name instead of into it, so nothing is merged and nothing is skipped.
    let mut top: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for i in &items {
        let first = i.rel.split('/').next().unwrap_or("");
        if top.contains_key(first) {
            continue;
        }
        let safe = dst.safe_name(first);
        let target = dst.join(dest_dir, &safe);
        let name = if opts.policy == LocalConflict::KeepBoth && dst.stat(&target)?.is_some() {
            let free = free_name(dst, &target, &opts.reuse)?;
            report.kept_both.push((target, free.clone()));
            free.rsplit(['/', '\\']).next().unwrap_or(&free).to_string()
        } else {
            safe
        };
        top.insert(first.to_string(), name);
    }

    let dest_path = |rel: &str, dst: &dyn Endpoint, renamed: &mut Vec<(String, String)>| -> String {
        let mut p = dest_dir.to_string();
        for (k, c) in rel.split('/').enumerate() {
            let safe = dst.safe_name(c);
            if safe != c && !renamed.iter().any(|(from, _)| from == c) {
                renamed.push((c.to_string(), safe.clone()));
            }
            p = dst.join(&p, if k == 0 { top.get(c).unwrap_or(&safe) } else { &safe });
        }
        p
    };

    // Folders first, in one batch. An existing folder is merged into.
    let mut dirs: Vec<(String, &WalkItem)> = Vec::new();
    for i in items.iter().filter(|i| i.stat.kind == FileKind::Dir) {
        dirs.push((dest_path(&i.rel, dst, &mut report.renamed), i));
    }
    let mut new_dirs = Vec::new();
    for (p, _) in &dirs {
        if dst.stat(p)?.is_none() {
            new_dirs.push(p.clone());
        }
    }
    dst.mkdirs(&dirs.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>())?;
    for p in new_dirs {
        on(&Event::Created { path: p, is_dir: true });
    }

    // Decide each file's destination up front.
    let mut done = 0u64;
    let mut todo: Vec<Planned> = Vec::new();
    // Move: files now at the destination, verified, whose originals go at the end.
    let mut moved: Vec<Planned> = Vec::new();
    for i in items.iter().filter(|i| i.stat.kind == FileKind::File) {
        let mut target = dest_path(&i.rel, dst, &mut report.renamed);
        let mut existed = false;
        if let Some(existing) = dst.stat(&target)? {
            existed = true;
            if existing.kind == FileKind::File && existing.size == i.stat.size && existing.mtime_ns == i.stat.mtime_ns {
                // A Move deletes the original, so a look-alike is read and compared first
                // (this is what lets a Move that was interrupted after copying carry on).
                let same = if opts.mode == Mode::Move {
                    on(&Event::Checking { path: i.path.clone() });
                    same_content(src, dst, &i.path, &target)?
                } else {
                    true
                };
                if same {
                    done += i.stat.size;
                    let why = "already there".to_string();
                    report.skipped.push((i.path.clone(), why.clone()));
                    on(&Event::Skipped { path: i.path.clone(), why });
                    on(&Event::Progress { done, total: report.bytes });
                    if opts.mode == Mode::Move {
                        moved.push(Planned { src: i.path.clone(), dst: target, stat: i.stat.clone(), existed: true });
                    }
                    continue;
                }
            }
            match opts.policy {
                LocalConflict::Skip => {
                    done += i.stat.size;
                    let why = "a different file is already there".to_string();
                    report.skipped.push((i.path.clone(), why.clone()));
                    on(&Event::Skipped { path: i.path.clone(), why });
                    on(&Event::Progress { done, total: report.bytes });
                    continue;
                }
                LocalConflict::Replace => {}
                LocalConflict::KeepBoth => {
                    target = free_name(dst, &target, &opts.reuse)?;
                    existed = false;
                }
            }
        }
        todo.push(Planned { src: i.path.clone(), dst: target, stat: i.stat.clone(), existed });
    }

    for p in &todo {
        if is_cancelled() {
            break;
        }
        on(&Event::Copying { path: p.src.clone(), to: p.dst.clone() });
        let mut attempt = 0;
        let result = loop {
            attempt += 1;
            let res = copy_one(src, dst, p, done, report.bytes, opts, on);
            match res {
                Err(Error::Verify(_)) if attempt <= opts.retries && !is_cancelled() => continue,
                other => break other,
            }
        };
        if is_cancelled() {
            report.cancelled = true;
            break;
        }
        match result {
            Ok(()) => {
                report.copied += 1;
                if !p.existed {
                    on(&Event::Created { path: p.dst.clone(), is_dir: false });
                }
                on(&Event::Copied { path: p.src.clone() });
                if opts.mode == Mode::Move {
                    moved.push(p.clone());
                }
            }
            Err(e) if e.is_lost() => return Err(e),
            Err(e) => {
                report.failed.push((p.src.clone(), e.to_string()));
                on(&Event::Failed { path: p.src.clone(), error: e.to_string() });
            }
        }
        done += p.stat.size;
        on(&Event::Progress { done, total: report.bytes });
    }
    report.cancelled = report.cancelled || is_cancelled();

    // Move: only now, with everything copied and verified, are the originals
    // deleted. This can't be stopped partway (a stop would then leave the only
    // copy of some files at the destination), and it is quick.
    if opts.mode == Mode::Move {
        if report.cancelled || !report.failed.is_empty() {
            report.originals_kept = !moved.is_empty();
        } else {
            for p in &moved {
                move_original(src, p, &mut report, on);
            }
            // Then the folders left empty, deepest first. A folder that still
            // holds something (skipped files, say) is left alone.
            let mut src_dirs: Vec<&WalkItem> = items.iter().filter(|i| i.stat.kind == FileKind::Dir).collect();
            src_dirs.sort_by_key(|i| std::cmp::Reverse(i.rel.matches('/').count()));
            for d in src_dirs {
                if src.remove(&d.path).is_ok() {
                    on(&Event::Removed { path: d.path.clone() });
                }
            }
        }
    }
    Ok(report)
}

/// Whether two files have the same content, by reading both.
fn same_content(src: &mut dyn Endpoint, dst: &mut dyn Endpoint, from: &str, to: &str) -> Result<bool> {
    let a = src.read(from, 0, &mut |_| Ok(()));
    let b = dst.read(to, 0, &mut |_| Ok(()));
    match (a, b) {
        (Ok(a), Ok(b)) => Ok(a == b),
        (Err(e), _) | (_, Err(e)) if e.is_lost() => Err(e),
        _ => Ok(false),
    }
}

fn copy_one(
    src: &mut dyn Endpoint,
    dst: &mut dyn Endpoint,
    p: &Planned,
    done: u64,
    total: u64,
    opts: &CopyOptions,
    on: &mut dyn FnMut(&Event),
) -> Result<()> {
    let is_cancelled = || opts.cancel.as_ref().is_some_and(|f| f.load(Ordering::Relaxed));
    // Continue a copy that was interrupted, if its part file is still there.
    let offset = match dst.stat(&part_string(&p.dst))? {
        Some(s) if s.kind == FileKind::File && s.size <= p.stat.size => s.size,
        _ => 0,
    };
    let begin = dst.begin_write(&p.dst, p.stat.size, p.stat.mtime_ns, p.stat.mode, offset);
    let offset = match begin {
        Ok(()) => offset,
        Err(_) if offset > 0 => {
            dst.begin_write(&p.dst, p.stat.size, p.stat.mtime_ns, p.stat.mode, 0)?;
            0
        }
        Err(e) => return Err(e),
    };
    let mut sent = offset;
    on(&Event::Progress { done: done + sent, total });
    let read = src.read(&p.src, offset, &mut |chunk| {
        if is_cancelled() {
            return Err(cancelled_error());
        }
        dst.write(chunk)?;
        sent += chunk.len() as u64;
        on(&Event::Progress { done: done + sent, total });
        Ok(())
    });
    match read {
        Ok(digest) => dst.finish_write(digest),
        Err(e) => {
            dst.abort_write();
            Err(e)
        }
    }
}

/// The part file's path for a destination path, on either kind of side.
pub fn part_string(target: &str) -> String {
    match target.rfind(['/', '\\']) {
        Some(i) => format!("{}{}.{}.qm-part", &target[..i], &target[i..=i], &target[i + 1..]),
        None => format!(".{target}.qm-part"),
    }
}

fn move_original(src: &mut dyn Endpoint, p: &Planned, report: &mut CopyReport, on: &mut dyn FnMut(&Event)) {
    let now = match src.stat(&p.src) {
        Ok(s) => s,
        Err(e) => {
            let why = format!("couldn't check it: {e}");
            report.kept.push((p.src.clone(), why.clone()));
            on(&Event::Kept { path: p.src.clone(), why });
            return;
        }
    };
    if now.as_ref().is_some_and(|n| n.same_file_as(&p.stat)) {
        match src.remove(&p.src) {
            Ok(()) => {
                report.removed += 1;
                on(&Event::Removed { path: p.src.clone() });
                // A Mac keeps a file's extra details in "._name" beside it on some disks, and
                // removes it with the file; elsewhere it would be left behind.
                if let Some((dir, name)) = p.src.rsplit_once(['/', '\\']) {
                    let _ = src.remove(&src.join(dir, &format!("._{name}")));
                }
            }
            Err(e) => {
                report.kept.push((p.src.clone(), e.to_string()));
                on(&Event::Kept { path: p.src.clone(), why: e.to_string() });
            }
        }
    } else {
        let why = "it changed while it was being copied".to_string();
        report.kept.push((p.src.clone(), why.clone()));
        on(&Event::Kept { path: p.src.clone(), why });
    }
}

/// "name (2)" beside `target`, or the next number that's free. A name an earlier attempt
/// of this copy already created (`reuse`) is used again.
fn free_name(dst: &mut dyn Endpoint, target: &str, reuse: &[String]) -> Result<String> {
    let sep = if target.contains('\\') && !target.contains('/') { '\\' } else { '/' };
    let (dir, name) = match target.rfind(sep) {
        Some(i) => (&target[..i], &target[i + 1..]),
        None => ("", target),
    };
    let mut n = 2;
    loop {
        let cand = dst.join(dir, &util::numbered_name(name, n));
        if reuse.contains(&cand) || dst.stat(&cand)?.is_none() {
            return Ok(cand);
        }
        n += 1;
    }
}

/// Delete `path` and everything under it (used to abandon a copy, on either
/// side). Children go before their folders.
pub fn remove_tree(side: &mut dyn Endpoint, path: &str) -> Result<()> {
    if side.stat(path)?.is_none() {
        return Ok(());
    }
    let walk = side.walk(path)?;
    for item in walk.items.iter().rev() {
        side.remove(&item.path)?;
    }
    Ok(())
}
