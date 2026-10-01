//! Wire protocol between a client (desktop app, CLI, or a file-server job) and
//! `archive-helper serve`, carried over an SSH channel's stdin/stdout.
//!
//! Every frame is `kind: u8, length: u32 LE, payload`. `Msg` frames carry one
//! JSON-encoded [`Request`] or [`Response`]. File bytes travel as `Data`
//! frames closed by an `End` frame (with a JSON [`Trailer`]) or an `Abort`
//! frame (with an error message).
//!
//! Upload: `Msg(PutFile)` → `Msg(Proceed)` (or a final answer, e.g. skipped),
//! then `Data`… `End{digest}` → `Msg(Outcome)`. The server always reads the
//! stream through to `End`/`Abort` before answering, so both sides stay in step.
//!
//! Download: `Msg(ReadFile)` → `Msg(FileStart{frames, skip, len})`, then one
//! `Data` per zstd frame and `End`. Large files travel as their stored frames;
//! a small file from a shared block is sent as its own frame. `ReadMany`
//! returns several such replies back to back.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::api::{Conflict, DirSpec, PutFile, PutOutcome, PutSolid};
use crate::catalog::{Entry, Pack};
use crate::copy::{FileStat, Walk};
use crate::error::Error;
use crate::fsview::{Listing, Measure, Place, SearchResult};
use crate::hash::Digest;
use crate::jobs::{ArchiveTarget, JobSpec, JobStatus};
use crate::keys::AuthorizedKey;
use crate::link::LinkTarget;
use crate::maint::Info;
use crate::trash::{Report as TrashReport, Trashed};
use crate::vpath::VPath;

pub const PROTOCOL_VERSION: u32 = 8;
/// Largest frame accepted, as a guard against a corrupted stream.
pub const MAX_FRAME: usize = 512 << 20;
/// Size of `Data` frames for uploads.
pub const CHUNK: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Msg = 1,
    Data = 2,
    End = 3,
    Abort = 4,
}

pub fn write_frame(w: &mut impl Write, kind: Kind, payload: &[u8]) -> io::Result<()> {
    let mut head = [0u8; 5];
    head[0] = kind as u8;
    head[1..5].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    w.write_all(&head)?;
    w.write_all(payload)
}

/// Read one frame into `buf`. Returns `None` at a clean end of stream.
pub fn read_frame(r: &mut impl Read, buf: &mut Vec<u8>) -> io::Result<Option<Kind>> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head[..1]) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    r.read_exact(&mut head[1..])?;
    let kind = match head[0] {
        1 => Kind::Msg,
        2 => Kind::Data,
        3 => Kind::End,
        4 => Kind::Abort,
        k => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unknown frame kind {k}"))),
    };
    let len = u32::from_le_bytes(head[1..5].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame of {len} bytes is too large")));
    }
    buf.resize(len, 0);
    r.read_exact(buf)?;
    Ok(Some(kind))
}

pub fn write_json<T: Serialize>(w: &mut impl Write, kind: Kind, v: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(v).map_err(io::Error::other)?;
    write_frame(w, kind, &bytes)
}

pub fn parse_json<T: DeserializeOwned>(buf: &[u8]) -> io::Result<T> {
    serde_json::from_slice(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Trailer {
    /// SHA-256 of the raw (uncompressed) content, for uploads.
    pub digest: Option<Digest>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InitOptions {
    pub pack_target_bytes: Option<u64>,
    pub par2_redundancy_percent: Option<u32>,
    pub trash_days: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", content = "args", rename_all = "snake_case")]
pub enum Request {
    Hello {
        version: u32,
        client: String,
    },
    Init {
        options: InitOptions,
    },
    Stat {
        path: VPath,
    },
    List {
        path: VPath,
    },
    Walk {
        path: VPath,
    },
    /// Deduplication is per project: `scope` is a path in the project.
    SizesPresent {
        scope: VPath,
        sizes: Vec<u64>,
    },
    Have {
        scope: VPath,
        hashes: Vec<Digest>,
    },
    Mkdirs {
        job: String,
        dirs: Vec<DirSpec>,
    },
    Symlink {
        job: String,
        dest: VPath,
        target: String,
        mtime_ns: i64,
        policy: Conflict,
    },
    Link {
        job: String,
        req: PutFile,
        sha256: Digest,
    },
    LinkMany {
        job: String,
        items: Vec<(PutFile, Digest)>,
    },
    PutFile {
        job: String,
        req: PutFile,
    },
    PutSolid {
        job: String,
        req: PutSolid,
    },
    /// Wait for the archive's checks of files sent so far; reply with any that failed.
    Settle,
    FinishJob {
        job: String,
    },
    ReadFile {
        path: VPath,
    },
    /// Several `ReadFile` replies back to back, one per path, in order.
    ReadMany {
        paths: Vec<VPath>,
    },
    Info {
        path: VPath,
    },
    Search {
        query: String,
        limit: usize,
    },
    CreateFolder {
        path: VPath,
    },
    Rename {
        from: VPath,
        to: VPath,
    },
    Trash {
        path: VPath,
    },
    Restore {
        id: i64,
        to: Option<VPath>,
    },
    TrashList,
    Packs,
    WorkerStatus,
    /// Start a background maintenance run now (PAR2, checks, purge).
    KickWorker,

    // Limited keys for file servers (archive servers only; not when restricted).
    KeyAuthorize {
        key: String,
        label: String,
        from: Option<String>,
    },
    KeyList,
    KeyRevoke {
        label: String,
    },

    // File servers (`archive-helper files`).
    FsList {
        path: Option<String>,
        show_hidden: bool,
    },
    FsSearch {
        root: String,
        query: String,
        every_file: bool,
        show_hidden: bool,
    },
    FsMeasure {
        paths: Vec<String>,
    },
    FsMkdir {
        path: String,
    },
    /// The home folder, its usual folders, and mounted drives (for the Places menu).
    FsPlaces,
    /// Plain file transfers (see [`crate::copy`]). `FsStat` answers `Stat`.
    FsStat {
        path: String,
    },
    /// Everything under `root`, links not followed. Answers `Walked`.
    FsWalk {
        root: String,
    },
    /// Create folders (and any missing parents); folders that exist are fine.
    FsMkdirs {
        paths: Vec<String>,
    },
    /// Remove a file or an empty folder, never anything recursively.
    FsRemove {
        path: String,
    },
    /// Send items to the helper's own Trash (see [`crate::trash`]): each is moved, instantly, and
    /// stays until restored or deleted for good. Answers `TrashReport`; a refused or missing item
    /// fails the whole request and nothing moves.
    FsTrash {
        paths: Vec<String>,
    },
    /// What is in the Trash on this server, newest first. Answers `Trashed`.
    FsTrashList,
    /// Put an item back where it came from (never overwriting). Answers `Text` with its path.
    FsTrashRestore {
        id: String,
    },
    /// Delete items in the Trash for good: those with these ids, or (`None`) all of them.
    /// Answers `Id(n)`.
    FsTrashEmpty {
        ids: Option<Vec<String>>,
    },
    /// Delete files and folders (with everything in a folder) immediately and for good; a file
    /// server has no Trash. Refuses the top of a disk, the home folder and anything holding it,
    /// mount points, and folders that hold another disk; a refused request deletes nothing.
    /// Answers `Id(n)`: how many of the items were deleted.
    FsDelete {
        paths: Vec<String>,
    },
    /// Download: `FsStart`, then `Data`… and `End{digest}` (the SHA-256 of the
    /// whole file, though only bytes from `offset` are sent) or `Abort`.
    FsRead {
        path: String,
        offset: u64,
    },
    /// Upload: answered `Proceed`, then the client sends `Data`… and
    /// `End{digest}` (of the whole file). The bytes go to a part file, which is
    /// verified against `digest` and renamed into place; an `Abort`, or a lost
    /// connection, keeps the part file so `offset` can continue it. Ends `Ok`.
    FsWrite {
        path: String,
        size: u64,
        mtime_ns: i64,
        mode: u32,
        offset: u64,
    },
    JobStart {
        spec: JobSpec,
    },
    /// All jobs; optionally restart any that were interrupted.
    JobList {
        resume_interrupted: bool,
    },
    JobCancel {
        id: String,
    },
    /// Stop a job at the next file, keeping its place.
    JobPause {
        id: String,
    },
    /// Continue a paused or interrupted job where it stopped.
    JobResume {
        id: String,
    },
    /// Abandon a stopped retrieve: delete what it created on this server.
    JobDiscard {
        id: String,
    },
    /// This server's transfer public key (created on first use).
    TransferKey,
    /// Remember an archive server's host key, in known_hosts format.
    TrustHosts {
        lines: Vec<String>,
    },
    /// Try reaching an archive the way a job would.
    RouteTest {
        target: ArchiveTarget,
    },
    /// Sign in to an archive server from this file server and keep the
    /// connection open for jobs (see [`crate::link`]). The server may reply
    /// with `Prompt`s first; answer each with `PromptAnswer`. Ends with `Ok`.
    SignIn {
        target: LinkTarget,
    },
    /// The user's answer to a `Prompt` (`None` cancels the sign-in).
    PromptAnswer {
        id: u64,
        answer: Option<String>,
    },
    /// Is this file server signed in to the archive server? (`Bools([open])`)
    LinkCheck {
        target: LinkTarget,
    },
    /// Close the signed-in connection.
    SignOut {
        target: LinkTarget,
    },
}

impl Request {
    /// Operations a restricted (server-to-server) key may use: reading and
    /// adding data, but never deleting, renaming, or reconfiguring.
    pub fn allowed_when_restricted(&self) -> bool {
        !matches!(
            self,
            Request::Init { .. }
                | Request::Rename { .. }
                | Request::Trash { .. }
                | Request::Restore { .. }
                | Request::TrashList
                | Request::KeyAuthorize { .. }
                | Request::KeyList
                | Request::KeyRevoke { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    NotFound,
    AlreadyExists,
    NotADirectory,
    InvalidPath,
    Verify,
    Corrupt,
    Par2,
    Denied,
    Other,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemResult {
    Ok(PutOutcome),
    Err { kind: ErrorKind, message: String },
}

impl ItemResult {
    pub fn from_result(r: crate::error::Result<PutOutcome>) -> ItemResult {
        match r {
            Ok(o) => ItemResult::Ok(o),
            Err(e) => match Response::from_error(&e) {
                Response::Error { kind, message } => ItemResult::Err { kind, message },
                _ => unreachable!(),
            },
        }
    }

    pub fn into_result(self) -> crate::error::Result<PutOutcome> {
        match self {
            ItemResult::Ok(o) => Ok(o),
            ItemResult::Err { kind, message } => Err(to_error(kind, message)),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HelloInfo {
    pub version: u32,
    pub helper: String,
    pub os: String,
    pub root: String,
    /// `None` if no archive exists at `root` yet (send `Init`).
    pub archive_id: Option<String>,
    pub restricted: bool,
    pub free_bytes: Option<u64>,
    /// The server's host name (to tell whether two connections reach the same machine).
    #[serde(default)]
    pub host: String,
    /// The address this connection came from, as the server sees it.
    #[serde(default)]
    pub client_ip: Option<String>,
}

impl HelloInfo {
    /// Filled in from the environment of the running helper.
    pub fn here(root: String, archive_id: Option<String>, restricted: bool, helper: String) -> HelloInfo {
        let client_ip = std::env::var("SSH_CLIENT").ok().and_then(|v| v.split_whitespace().next().map(str::to_string));
        HelloInfo {
            version: PROTOCOL_VERSION,
            helper,
            os: std::env::consts::OS.to_string(),
            free_bytes: crate::util::free_space(std::path::Path::new(&root)),
            root,
            archive_id,
            restricted,
            host: crate::util::hostname(),
            client_ip,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "r", content = "v", rename_all = "snake_case")]
pub enum Response {
    Hello(HelloInfo),
    Ok,
    Proceed,
    Entry(Option<Entry>),
    Entries(Vec<Entry>),
    Walk(Vec<(String, Entry)>),
    Bools(Vec<bool>),
    Outcome(PutOutcome),
    Outcomes(Vec<PutOutcome>),
    /// Files whose check after writing failed (destination, reason).
    Failed(Vec<(VPath, String)>),
    /// One result per item, for batched requests.
    Results(Vec<ItemResult>),
    FileStart {
        entry: Entry,
        frames: Vec<u32>,
        skip: u64,
        len: u64,
    },
    Info(Info),
    Search(Vec<(VPath, Entry)>),
    Id(i64),
    Path(VPath),
    Packs(Vec<Pack>),
    Json(serde_json::Value),
    Text(String),
    Listing(Listing),
    Places(Vec<Place>),
    Stat(Option<FileStat>),
    TrashReport(TrashReport),
    Trashed(Vec<Trashed>),
    Walked(Walk),
    /// A download is starting.
    FsStart {
        size: u64,
    },
    Found(SearchResult),
    Measure(Measure),
    Jobs(Vec<JobStatus>),
    Keys(Vec<AuthorizedKey>),
    /// A question from a sign-in in progress (a password, a two-factor code or choice, or a
    /// yes/no about an unknown host), to show the user.
    Prompt {
        id: u64,
        text: String,
    },
    Error {
        kind: ErrorKind,
        message: String,
    },
}

impl Response {
    pub fn from_error(e: &Error) -> Response {
        let (kind, message) = match e {
            Error::NotFound(m) => (ErrorKind::NotFound, m.clone()),
            Error::AlreadyExists(m) => (ErrorKind::AlreadyExists, m.clone()),
            Error::NotADirectory(m) => (ErrorKind::NotADirectory, m.clone()),
            Error::InvalidPath(m) => (ErrorKind::InvalidPath, m.clone()),
            Error::Verify(m) => (ErrorKind::Verify, m.clone()),
            Error::Corrupt(m) => (ErrorKind::Corrupt, m.clone()),
            Error::Par2(m) => (ErrorKind::Par2, m.clone()),
            other => (ErrorKind::Other, other.to_string()),
        };
        Response::Error { kind, message }
    }
}

pub fn to_error(kind: ErrorKind, message: String) -> Error {
    match kind {
        ErrorKind::NotFound => Error::NotFound(message),
        ErrorKind::AlreadyExists => Error::AlreadyExists(message),
        ErrorKind::NotADirectory => Error::NotADirectory(message),
        ErrorKind::InvalidPath => Error::InvalidPath(message),
        ErrorKind::Verify => Error::Verify(message),
        ErrorKind::Corrupt => Error::Corrupt(message),
        ErrorKind::Par2 => Error::Par2(message),
        ErrorKind::Denied => Error::Other(format!("not allowed: {message}")),
        ErrorKind::Other => Error::Other(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_json(&mut buf, Kind::Msg, &Request::Stat { path: VPath::parse("/a b/é").unwrap() }).unwrap();
        write_frame(&mut buf, Kind::Data, b"xyz").unwrap();
        write_json(&mut buf, Kind::End, &Trailer { digest: Some(Digest::of(b"xyz")) }).unwrap();
        let mut r = Cursor::new(buf);
        let mut f = Vec::new();
        assert_eq!(read_frame(&mut r, &mut f).unwrap(), Some(Kind::Msg));
        let req: Request = parse_json(&f).unwrap();
        assert!(matches!(req, Request::Stat { path } if path.to_string() == "/a b/é"));
        assert_eq!(read_frame(&mut r, &mut f).unwrap(), Some(Kind::Data));
        assert_eq!(f, b"xyz");
        assert_eq!(read_frame(&mut r, &mut f).unwrap(), Some(Kind::End));
        let t: Trailer = parse_json(&f).unwrap();
        assert_eq!(t.digest, Some(Digest::of(b"xyz")));
        assert_eq!(read_frame(&mut r, &mut f).unwrap(), None);
    }

    #[test]
    fn responses_serialize() {
        for r in [Response::Ok, Response::Bools(vec![true]), Response::Entry(None), Response::Id(5)] {
            let s = serde_json::to_string(&r).unwrap();
            let _: Response = serde_json::from_str(&s).unwrap();
        }
    }
}
