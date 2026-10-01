//! The interface the transfer engine uses to talk to an archive. The local
//! [`crate::store::Store`] implements it directly; a remote archive (over SSH)
//! implements the same trait by forwarding each call.

use std::io::Read;

use serde::{Deserialize, Serialize};

use crate::catalog::Entry;
use crate::error::Result;
use crate::hash::Digest;
use crate::maint::Info;
use crate::seekable::CompressingReader;
use crate::vpath::VPath;

/// What to do when the destination name already exists with different content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Conflict {
    /// Leave the existing item alone and don't archive the new one.
    #[default]
    Skip,
    /// Move the existing item to the trash and store the new one. Archived
    /// projects are frozen, so this is refused unless the existing file was
    /// stored earlier in the same transfer (a file that changed while it was
    /// read is sent again this way).
    Replace,
    /// Store the new one as "name (2).ext".
    KeepBoth,
    /// Stop with an error.
    Fail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirSpec {
    pub path: VPath,
    pub mtime_ns: i64,
    pub mode: u32,
    /// This folder is what was sent, so it becomes a project, unless it lands
    /// inside a project (then it's added to that project).
    #[serde(default)]
    pub project: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PutFile {
    pub dest: VPath,
    pub meta: FileMeta,
    pub policy: Conflict,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SolidMember {
    pub dest: VPath,
    pub meta: FileMeta,
    pub sha256: Digest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PutSolid {
    pub members: Vec<SolidMember>,
    pub policy: Conflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PutStatus {
    /// New bytes were written and verified.
    Stored,
    /// The content already existed in the archive; only a reference was added.
    Deduplicated,
    /// The destination already held exactly this content.
    Identical,
    /// The destination held different content and the policy said skip.
    /// The source is NOT archived and must not be deleted.
    Skipped,
}

impl PutStatus {
    /// True if the archive now holds this content at the destination, so a
    /// Move may delete the source.
    pub fn is_archived(self) -> bool {
        !matches!(self, PutStatus::Skipped)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutOutcome {
    /// Final destination (differs from the request for keep-both renames).
    pub dest: VPath,
    pub status: PutStatus,
}

/// Callback for [`Archive::read_files`]: called with each file's index and
/// either its entry and a reader for its bytes, or the error for that file.
pub type ReadEach<'a> = dyn FnMut(usize, Result<(Entry, &mut dyn Read)>) -> Result<()> + 'a;

/// A compressed payload whose raw-content digest is known once fully read.
pub trait Payload: Read {
    fn digest(&self) -> Option<Digest>;
}

impl<R: Read> Payload for CompressingReader<R> {
    fn digest(&self) -> Option<Digest> {
        CompressingReader::digest(self)
    }
}

pub trait Archive {
    /// Wait for the archive's checks of files already sent, and return any that
    /// failed (destination, reason); those weren't kept, so send them again.
    /// Archives that check each file before answering have nothing to report.
    fn settle(&mut self) -> Result<Vec<(VPath, String)>> {
        Ok(Vec::new())
    }
    fn stat(&mut self, path: &VPath) -> Result<Option<Entry>>;
    fn list(&mut self, path: &VPath) -> Result<Vec<Entry>>;
    /// Every node under a folder, with paths relative to it.
    fn walk(&mut self, path: &VPath) -> Result<Vec<(String, Entry)>>;
    /// For each size, whether any content stored in the project at `scope`
    /// (a path in or below the project's top folder) has exactly that size.
    fn sizes_present(&mut self, scope: &VPath, sizes: &[u64]) -> Result<Vec<bool>>;
    /// For each digest, whether that content is already stored in the project at `scope`.
    fn have(&mut self, scope: &VPath, hashes: &[Digest]) -> Result<Vec<bool>>;

    /// Create folders (and missing parents). Batched so a big tree costs one fsync.
    fn mkdirs(&mut self, job: &str, dirs: &[DirSpec]) -> Result<()>;
    fn symlink(&mut self, job: &str, dest: &VPath, target: &str, mtime_ns: i64, policy: Conflict) -> Result<PutOutcome>;
    /// Reference already-stored content at `dest` (deduplication).
    fn link(&mut self, job: &str, req: &PutFile, sha256: &Digest) -> Result<PutOutcome>;
    /// Many links in one exchange; one result per item, in order.
    fn link_many(&mut self, job: &str, items: &[(PutFile, Digest)]) -> Result<Vec<Result<PutOutcome>>> {
        Ok(items.iter().map(|(req, sha)| self.link(job, req, sha)).collect())
    }
    fn put_file(&mut self, job: &str, req: &PutFile, payload: &mut dyn Payload) -> Result<PutOutcome>;
    fn put_solid(&mut self, job: &str, req: &PutSolid, payload: &mut dyn Payload) -> Result<Vec<PutOutcome>>;
    /// Seal the job's open pack so it can be protected with PAR2.
    fn finish_job(&mut self, job: &str) -> Result<()>;

    /// Stream a file's original bytes.
    fn read_file(&mut self, path: &VPath) -> Result<(Entry, Box<dyn Read + Send + '_>)>;

    /// Stream several files in one exchange (a remote archive sends them back
    /// to back, avoiding a round trip per file). `each` is called once per
    /// path, in order; after it returns an error, later files are still
    /// consumed but not passed to it.
    fn read_files(&mut self, paths: &[VPath], each: &mut ReadEach<'_>) -> Result<()> {
        let mut stopped = false;
        for (i, p) in paths.iter().enumerate() {
            if stopped {
                break;
            }
            let res = match self.read_file(p) {
                Ok((e, mut r)) => each(i, Ok((e, &mut *r))),
                Err(e) => each(i, Err(e)),
            };
            stopped = res.is_err();
        }
        Ok(())
    }

    // Browsing and editing, as the desktop app uses them.

    /// Sizes, space used, duplicates, and protection status (the Get info panel).
    fn info(&mut self, path: &VPath) -> Result<Info>;
    fn search(&mut self, query: &str, limit: usize) -> Result<Vec<(VPath, Entry)>>;
    /// Create a folder and any missing parents (no pack entry; user action).
    fn create_folder(&mut self, path: &VPath) -> Result<()>;
    fn rename(&mut self, from: &VPath, to: &VPath) -> Result<()>;
    /// Move to the trash; returns the trash id used to restore it.
    fn trash(&mut self, path: &VPath) -> Result<i64>;
    fn restore(&mut self, trash_id: i64, to: Option<&VPath>) -> Result<VPath>;
    fn trash_list(&mut self) -> Result<Vec<Entry>>;
}
