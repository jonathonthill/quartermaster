//! Client side of the protocol: an [`Archive`] backed by `archive-helper
//! serve` on another machine, reached through the system `ssh` (so the
//! user's keys, ~/.ssh/config, and control sockets all apply) or any pipe.

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::api::{Archive, Conflict, DirSpec, Payload, PutFile, PutOutcome, PutSolid, ReadEach};
use crate::catalog::{Entry, Pack};
use crate::error::{Error, Result};
use crate::hash::Digest;
use crate::link::LinkTarget;
use crate::maint::Info;
use crate::proto::{self, HelloInfo, InitOptions, Kind, Request, Response, Trailer};
use crate::vpath::VPath;

pub const DEFAULT_HELPER: &str = "~/.local/bin/archive-helper";

/// Where to find an archive over SSH: `ssh://[user@]host[:port]/path`.
/// A path starting with `/~/` is relative to the remote home folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshTarget {
    pub host: String,
    pub port: Option<u16>,
    pub root: String,
    pub helper: String,
    /// A file server (`archive-helper files`) rather than an archive.
    pub files: bool,
}

impl SshTarget {
    pub fn parse(url: &str) -> Option<SshTarget> {
        let rest = url.strip_prefix("ssh://")?;
        let slash = rest.find('/')?;
        let (authority, path) = rest.split_at(slash);
        if authority.is_empty() {
            return None;
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p.parse().ok()?)),
            None => (authority.to_string(), None),
        };
        let root = path.strip_prefix("/~/").map(|r| format!("~/{r}")).unwrap_or_else(|| path.to_string());
        Some(SshTarget { host, port, root, helper: DEFAULT_HELPER.to_string(), files: false })
    }
}

/// Extra settings for the `ssh` processes a client starts.
#[derive(Clone, Debug, Default)]
pub struct SshSetup {
    /// Options placed before the host, e.g. `-o ControlMaster=auto`.
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// The ssh program: `ssh` from the PATH, unless `ARCHIVE_HELPER_SSH` names
/// another (tests use a stand-in).
pub fn ssh_program() -> std::ffi::OsString {
    std::env::var_os("ARCHIVE_HELPER_SSH").unwrap_or_else(|| "ssh".into())
}

impl SshSetup {
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(ssh_program());
        cmd.args(&self.args);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd
    }
}

/// Run one command on `host` through ssh, feeding it `input`; returns stdout.
pub fn run_ssh(host: &str, port: Option<u16>, setup: &SshSetup, command: &str, input: Option<&[u8]>) -> Result<String> {
    let mut cmd = setup.command();
    cmd.arg("-T");
    if let Some(p) = port {
        cmd.args(["-p", &p.to_string()]);
    }
    let mut child = cmd
        .arg(host)
        .arg(command)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::other(format!("can't run ssh: {e}")))?;
    if let Some(data) = input {
        child.stdin.take().unwrap().write_all(data)?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(Error::other(if err.is_empty() { format!("the command failed on {host}") } else { err }));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Upload the right `archive-helper` build from `helpers` (files named
/// `archive-helper-<os>-<arch>`) to `~/.local/bin` on `host`, verifying its
/// SHA-256 there before it replaces any older copy. Returns its version line.
pub fn install_helper(host: &str, port: Option<u16>, setup: &SshSetup, helpers: &std::path::Path) -> Result<String> {
    let uname = run_ssh(host, port, setup, "uname -sm", None)?;
    let (os, arch) = match uname.split_whitespace().collect::<Vec<_>>()[..] {
        [os, arch] => (os.to_ascii_lowercase(), if arch == "amd64" { "x86_64".to_string() } else { arch.replace("arm64", "aarch64") }),
        _ => return Err(Error::other(format!("unexpected `uname -sm` output from {host}: {uname}"))),
    };
    let path = helpers.join(format!("archive-helper-{os}-{arch}"));
    let data =
        std::fs::read(&path).map_err(|_| Error::NotFound(format!("no archive-helper build for {os}-{arch} ({})", path.display())))?;
    let want = Digest::of(&data).to_hex();
    let tmp = "~/.local/bin/.archive-helper.new";
    let got = run_ssh(
        host,
        port,
        setup,
        &format!("mkdir -p ~/.local/bin && cat > {tmp} && chmod 755 {tmp} && (sha256sum {tmp} 2>/dev/null || sha256 -r {tmp})"),
        Some(&data),
    )?;
    if got.split_whitespace().next() != Some(want.as_str()) {
        let _ = run_ssh(host, port, setup, &format!("rm -f {tmp}"), None);
        return Err(Error::Verify("the uploaded helper's checksum didn't match; nothing was installed".into()));
    }
    let version =
        run_ssh(host, port, setup, &format!("mv -f {tmp} ~/.local/bin/archive-helper && ~/.local/bin/archive-helper --version"), None)?;
    Ok(version.trim().to_string())
}

/// An ssh command that runs `remote_cmd` on `host` for a protocol session:
/// no terminal, and keepalives so a dead connection is noticed.
pub fn ssh_command(host: &str, port: Option<u16>, setup: &SshSetup, remote_cmd: &str) -> Command {
    let mut cmd = setup.command();
    cmd.args(["-T", "-o", "ServerAliveInterval=30", "-o", "ServerAliveCountMax=4"]);
    if let Some(p) = port {
        cmd.args(["-p", &p.to_string()]);
    }
    cmd.arg(host).arg(remote_cmd);
    cmd
}

/// The remote command that starts the helper session for `target`. The
/// helper path may start with ~ (expanded by the remote shell), so it is not
/// quoted; the root is quoted and the helper expands its ~.
pub fn helper_command(target: &SshTarget) -> String {
    if target.files {
        format!("{} files --start {}", target.helper, shell_quote(if target.root.is_empty() { "~" } else { &target.root }))
    } else {
        format!("{} serve --root {}", target.helper, shell_quote(&target.root))
    }
}

/// Quote for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub struct Remote {
    r: BufReader<Box<dyn Read + Send>>,
    w: BufWriter<Box<dyn Write + Send>>,
    buf: Vec<u8>,
    hello: HelloInfo,
    child: Option<Child>,
    stderr: Arc<Mutex<String>>,
}

impl Remote {
    /// Speak the protocol over an already-connected pair of streams.
    pub fn connect(r: Box<dyn Read + Send>, w: Box<dyn Write + Send>) -> Result<Remote> {
        let mut remote = Remote {
            r: BufReader::with_capacity(1 << 20, r),
            w: BufWriter::with_capacity(1 << 20, w),
            buf: Vec::new(),
            hello: blank_hello(),
            child: None,
            stderr: Arc::new(Mutex::new(String::new())),
        };
        remote.greet()?;
        Ok(remote)
    }

    /// Start `archive-helper serve` on `target.host` with the system ssh.
    pub fn ssh(target: &SshTarget) -> Result<Remote> {
        Self::ssh_with(target, &SshSetup::default())
    }

    /// Like [`Remote::ssh`], with extra ssh options and environment (the
    /// desktop app uses these for password prompts and shared connections).
    pub fn ssh_with(target: &SshTarget, setup: &SshSetup) -> Result<Remote> {
        let mut cmd = ssh_command(&target.host, target.port, setup, &helper_command(target));
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| Error::other(format!("can't run ssh: {e}")))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut err = child.stderr.take().unwrap();
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = stderr.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = err.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if let Ok(mut s) = sink.lock() {
                    s.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    if s.len() > 16_384 {
                        let cut = s.len() - 8192;
                        *s = s[cut..].to_string();
                    }
                }
            }
        });
        let mut remote = Remote {
            r: BufReader::with_capacity(1 << 20, Box::new(stdout)),
            w: BufWriter::with_capacity(1 << 20, Box::new(stdin)),
            buf: Vec::new(),
            hello: blank_hello(),
            child: Some(child),
            stderr,
        };
        remote.greet()?;
        Ok(remote)
    }

    pub fn hello(&self) -> &HelloInfo {
        &self.hello
    }

    fn greet(&mut self) -> Result<()> {
        let req = Request::Hello { version: proto::PROTOCOL_VERSION, client: format!("archive-core {}", env!("CARGO_PKG_VERSION")) };
        match self.call(&req)? {
            Response::Hello(h) => {
                self.hello = h;
                Ok(())
            }
            other => Err(unexpected(other)),
        }
    }

    fn lost(&self, e: io::Error) -> Error {
        let tail = self.stderr.lock().map(|s| s.trim().to_string()).unwrap_or_default();
        let hint =
            if tail.contains("not found") || tail.contains("No such file") { " (is archive-helper installed on the server?)" } else { "" };
        if tail.is_empty() {
            Error::other(format!("lost the connection to the server: {e}"))
        } else {
            Error::other(format!("lost the connection to the server: {tail}{hint}"))
        }
    }

    fn send(&mut self, req: &Request) -> Result<()> {
        proto::write_json(&mut self.w, Kind::Msg, req).and_then(|_| self.w.flush()).map_err(|e| self.lost(e))
    }

    fn recv(&mut self) -> Result<Response> {
        match proto::read_frame(&mut self.r, &mut self.buf) {
            Ok(Some(Kind::Msg)) => {}
            Ok(Some(k)) => return Err(Error::other(format!("protocol error: expected a reply, got {k:?}"))),
            Ok(None) => return Err(self.lost(io::Error::new(io::ErrorKind::UnexpectedEof, "closed"))),
            Err(e) => return Err(self.lost(e)),
        }
        let resp: Response = proto::parse_json(&self.buf)?;
        match resp {
            Response::Error { kind, message } => Err(proto::to_error(kind, message)),
            r => Ok(r),
        }
    }

    fn call(&mut self, req: &Request) -> Result<Response> {
        self.send(req)?;
        self.recv()
    }

    fn expect_ok(&mut self, req: &Request) -> Result<()> {
        match self.call(req)? {
            Response::Ok => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Create the archive at the connection's root.
    pub fn init(&mut self, options: InitOptions) -> Result<()> {
        self.expect_ok(&Request::Init { options })?;
        self.greet()
    }

    pub fn packs(&mut self) -> Result<Vec<Pack>> {
        match self.call(&Request::Packs)? {
            Response::Packs(p) => Ok(p),
            other => Err(unexpected(other)),
        }
    }

    pub fn worker_status(&mut self) -> Result<Value> {
        match self.call(&Request::WorkerStatus)? {
            Response::Json(v) => Ok(v),
            other => Err(unexpected(other)),
        }
    }

    pub fn kick_worker(&mut self) -> Result<()> {
        self.expect_ok(&Request::KickWorker)
    }

    // Limited keys (on an archive connection).

    pub fn key_authorize(&mut self, key: &str, label: &str, from: Option<&str>) -> Result<()> {
        self.expect_ok(&Request::KeyAuthorize { key: key.to_string(), label: label.to_string(), from: from.map(str::to_string) })
    }

    pub fn key_list(&mut self) -> Result<Vec<crate::keys::AuthorizedKey>> {
        match self.call(&Request::KeyList)? {
            Response::Keys(k) => Ok(k),
            other => Err(unexpected(other)),
        }
    }

    pub fn key_revoke(&mut self, label: &str) -> Result<()> {
        self.expect_ok(&Request::KeyRevoke { label: label.to_string() })
    }

    // File servers.

    pub fn fs_list(&mut self, path: Option<&str>, show_hidden: bool) -> Result<crate::fsview::Listing> {
        match self.call(&Request::FsList { path: path.map(str::to_string), show_hidden })? {
            Response::Listing(l) => Ok(l),
            other => Err(unexpected(other)),
        }
    }

    pub fn fs_search(&mut self, root: &str, query: &str, every_file: bool, show_hidden: bool) -> Result<crate::fsview::SearchResult> {
        match self.call(&Request::FsSearch { root: root.to_string(), query: query.to_string(), every_file, show_hidden })? {
            Response::Found(r) => Ok(r),
            other => Err(unexpected(other)),
        }
    }

    pub fn fs_measure(&mut self, paths: &[String]) -> Result<crate::fsview::Measure> {
        match self.call(&Request::FsMeasure { paths: paths.to_vec() })? {
            Response::Measure(m) => Ok(m),
            other => Err(unexpected(other)),
        }
    }

    pub fn fs_places(&mut self) -> Result<Vec<crate::fsview::Place>> {
        match self.call(&Request::FsPlaces)? {
            Response::Places(p) => Ok(p),
            other => Err(unexpected(other)),
        }
    }

    pub fn fs_mkdir(&mut self, path: &str) -> Result<()> {
        self.expect_ok(&Request::FsMkdir { path: path.to_string() })
    }

    // Plain file transfers (see `crate::copy`).

    pub fn fs_stat(&mut self, path: &str) -> Result<Option<crate::copy::FileStat>> {
        match self.call(&Request::FsStat { path: path.to_string() })? {
            Response::Stat(s) => Ok(s),
            other => Err(unexpected(other)),
        }
    }

    pub fn fs_walk(&mut self, root: &str) -> Result<crate::copy::Walk> {
        match self.call(&Request::FsWalk { root: root.to_string() })? {
            Response::Walked(w) => Ok(w),
            other => Err(unexpected(other)),
        }
    }

    pub fn fs_mkdirs(&mut self, paths: &[String]) -> Result<()> {
        self.expect_ok(&Request::FsMkdirs { paths: paths.to_vec() })
    }

    pub fn fs_remove(&mut self, path: &str) -> Result<()> {
        self.expect_ok(&Request::FsRemove { path: path.to_string() })
    }

    /// Send items to the Trash on this server (see [`crate::trash`]).
    pub fn fs_trash(&mut self, paths: &[String]) -> Result<crate::trash::Report> {
        match self.call(&Request::FsTrash { paths: paths.to_vec() })? {
            Response::TrashReport(r) => Ok(r),
            other => Err(unexpected(other)),
        }
    }

    pub fn fs_trash_list(&mut self) -> Result<Vec<crate::trash::Trashed>> {
        match self.call(&Request::FsTrashList)? {
            Response::Trashed(t) => Ok(t),
            other => Err(unexpected(other)),
        }
    }

    /// Put an item back; returns where it went.
    pub fn fs_trash_restore(&mut self, id: &str) -> Result<String> {
        match self.call(&Request::FsTrashRestore { id: id.to_string() })? {
            Response::Text(p) => Ok(p),
            other => Err(unexpected(other)),
        }
    }

    /// Delete items in the Trash for good (those with these ids, or all); returns how many.
    pub fn fs_trash_empty(&mut self, ids: Option<&[String]>) -> Result<usize> {
        match self.call(&Request::FsTrashEmpty { ids: ids.map(<[String]>::to_vec) })? {
            Response::Id(n) => Ok(n as usize),
            other => Err(unexpected(other)),
        }
    }

    /// Delete items on this server immediately and for good (see `Request::FsDelete`).
    pub fn fs_delete(&mut self, paths: &[String]) -> Result<usize> {
        match self.call(&Request::FsDelete { paths: paths.to_vec() })? {
            Response::Id(n) => Ok(n as usize),
            other => Err(unexpected(other)),
        }
    }

    /// Download `path` from `offset`, handing each chunk to `sink`. Returns the
    /// SHA-256 of the whole file. If `sink` fails the rest of the download is
    /// read and dropped, so the connection stays usable (except when the copy
    /// was cancelled: then the connection is abandoned).
    pub fn fs_read(&mut self, path: &str, offset: u64, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<Digest> {
        match self.call(&Request::FsRead { path: path.to_string(), offset })? {
            Response::FsStart { .. } => {}
            other => return Err(unexpected(other)),
        }
        let mut failed: Option<Error> = None;
        loop {
            match proto::read_frame(&mut self.r, &mut self.buf) {
                Ok(Some(Kind::Data)) => {
                    if failed.is_none() {
                        if let Err(e) = sink(&self.buf) {
                            // A cancelled copy is abandoning this connection anyway,
                            // so don't read the rest of a possibly huge file.
                            if matches!(&e, Error::Other(m) if m == "cancelled") {
                                return Err(e);
                            }
                            failed = Some(e);
                        }
                    }
                }
                Ok(Some(Kind::End)) => {
                    let t: Trailer = proto::parse_json(&self.buf)?;
                    return match failed {
                        Some(e) => Err(e),
                        None => t.digest.ok_or_else(|| Error::other("the server sent no checksum")),
                    };
                }
                Ok(Some(Kind::Abort)) => {
                    let msg = String::from_utf8_lossy(&self.buf).into_owned();
                    return Err(failed.unwrap_or_else(|| Error::other(msg)));
                }
                Ok(Some(k)) => return Err(Error::other(format!("protocol error: unexpected {k:?} in a download"))),
                Ok(None) => return Err(self.lost(io::Error::new(io::ErrorKind::UnexpectedEof, "closed"))),
                Err(e) => return Err(self.lost(e)),
            }
        }
    }

    /// Start an upload; the server answers `Proceed` once the part file is ready.
    pub fn fs_write_begin(&mut self, path: &str, size: u64, mtime_ns: i64, mode: u32, offset: u64) -> Result<()> {
        match self.call(&Request::FsWrite { path: path.to_string(), size, mtime_ns, mode, offset })? {
            Response::Proceed => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    pub fn fs_write_data(&mut self, data: &[u8]) -> Result<()> {
        proto::write_frame(&mut self.w, Kind::Data, data).map_err(|e| self.lost(e))
    }

    /// Finish an upload with the SHA-256 of the whole file; the server verifies and renames.
    pub fn fs_write_end(&mut self, digest: Digest) -> Result<()> {
        proto::write_json(&mut self.w, Kind::End, &Trailer { digest: Some(digest) }).map_err(|e| self.lost(e))?;
        self.w.flush().map_err(|e| self.lost(e))?;
        match self.recv()? {
            Response::Ok => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Stop an upload, keeping the server's part file so it can be continued.
    pub fn fs_write_abort(&mut self) -> Result<()> {
        proto::write_frame(&mut self.w, Kind::Abort, b"cancelled").map_err(|e| self.lost(e))?;
        self.w.flush().map_err(|e| self.lost(e))?;
        let _ = self.recv(); // the server's reply (an error: the upload was cut short)
        Ok(())
    }

    pub fn job_start(&mut self, spec: &crate::jobs::JobSpec) -> Result<String> {
        match self.call(&Request::JobStart { spec: spec.clone() })? {
            Response::Text(id) => Ok(id),
            other => Err(unexpected(other)),
        }
    }

    pub fn job_list(&mut self, resume_interrupted: bool) -> Result<Vec<crate::jobs::JobStatus>> {
        match self.call(&Request::JobList { resume_interrupted })? {
            Response::Jobs(j) => Ok(j),
            other => Err(unexpected(other)),
        }
    }

    pub fn job_cancel(&mut self, id: &str) -> Result<()> {
        self.expect_ok(&Request::JobCancel { id: id.to_string() })
    }

    pub fn job_pause(&mut self, id: &str) -> Result<()> {
        self.expect_ok(&Request::JobPause { id: id.to_string() })
    }

    pub fn job_resume(&mut self, id: &str) -> Result<()> {
        self.expect_ok(&Request::JobResume { id: id.to_string() })
    }

    pub fn job_discard(&mut self, id: &str) -> Result<()> {
        self.expect_ok(&Request::JobDiscard { id: id.to_string() })
    }

    pub fn transfer_key(&mut self) -> Result<String> {
        match self.call(&Request::TransferKey)? {
            Response::Text(k) => Ok(k),
            other => Err(unexpected(other)),
        }
    }

    pub fn trust_hosts(&mut self, lines: &[String]) -> Result<()> {
        self.expect_ok(&Request::TrustHosts { lines: lines.to_vec() })
    }

    /// Reach an archive from this file server, the way its jobs will.
    pub fn route_test(&mut self, target: &crate::jobs::ArchiveTarget) -> Result<HelloInfo> {
        match self.call(&Request::RouteTest { target: target.clone() })? {
            Response::Hello(h) => Ok(h),
            other => Err(unexpected(other)),
        }
    }

    /// Have this file server sign in to an archive server and keep the
    /// connection open for its jobs. Each question the sign-in asks goes to
    /// `ask` (return `None` to cancel). Returns at once if already signed in.
    pub fn sign_in(&mut self, target: &LinkTarget, ask: &mut dyn FnMut(&str) -> Option<String>) -> Result<()> {
        self.send(&Request::SignIn { target: target.clone() })?;
        loop {
            match self.recv()? {
                Response::Prompt { id, text } => {
                    let answer = ask(&text);
                    self.send(&Request::PromptAnswer { id, answer })?;
                }
                Response::Ok => return Ok(()),
                other => return Err(unexpected(other)),
            }
        }
    }

    /// Is this file server signed in to the archive server?
    pub fn link_check(&mut self, target: &LinkTarget) -> Result<bool> {
        match self.call(&Request::LinkCheck { target: target.clone() })? {
            Response::Bools(b) => Ok(b.first().copied().unwrap_or(false)),
            other => Err(unexpected(other)),
        }
    }

    pub fn sign_out(&mut self, target: &LinkTarget) -> Result<()> {
        self.expect_ok(&Request::SignOut { target: target.clone() })
    }

    /// Send a payload as `Data` frames and `End`, or `Abort` if reading it fails.
    fn stream(&mut self, payload: &mut dyn Payload) -> Result<()> {
        let mut chunk = vec![0u8; proto::CHUNK];
        loop {
            let n = match payload.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    proto::write_frame(&mut self.w, Kind::Abort, e.to_string().as_bytes()).map_err(|e| self.lost(e))?;
                    self.w.flush().map_err(|e| self.lost(e))?;
                    let _ = self.recv(); // the server's (error) reply
                    return Err(Error::Io(e));
                }
            };
            proto::write_frame(&mut self.w, Kind::Data, &chunk[..n]).map_err(|e| self.lost(e))?;
        }
        proto::write_json(&mut self.w, Kind::End, &Trailer { digest: payload.digest() }).map_err(|e| self.lost(e))?;
        self.w.flush().map_err(|e| self.lost(e))
    }
}

fn blank_hello() -> HelloInfo {
    HelloInfo {
        version: 0,
        helper: String::new(),
        os: String::new(),
        root: String::new(),
        archive_id: None,
        restricted: false,
        free_bytes: None,
        host: String::new(),
        client_ip: None,
    }
}

fn unexpected(r: Response) -> Error {
    Error::other(format!("protocol error: unexpected reply {:?}", std::mem::discriminant(&r)))
}

impl Drop for Remote {
    fn drop(&mut self) {
        let _ = self.w.flush();
        if let Some(mut c) = self.child.take() {
            // Closing stdin ends the helper; then reap ssh.
            drop(std::mem::replace(&mut self.w, BufWriter::new(Box::new(io::sink()))));
            let _ = c.wait();
        }
    }
}

/// Decompresses the stored frames of a `ReadFile` reply as they arrive.
struct FrameReader<'a> {
    remote: &'a mut Remote,
    frames: Vec<u32>,
    next: usize,
    skip: u64,
    remaining: u64,
    cur: Vec<u8>,
    pos: usize,
    done: bool,
}

impl FrameReader<'_> {
    fn finish(&mut self) -> io::Result<()> {
        // Consume any frames not yet read, then the End.
        while !self.done {
            match proto::read_frame(&mut self.remote.r, &mut self.remote.buf)? {
                Some(Kind::Data) => {}
                Some(Kind::End) => self.done = true,
                Some(Kind::Abort) => {
                    self.done = true;
                    return Err(io::Error::other(String::from_utf8_lossy(&self.remote.buf).into_owned()));
                }
                _ => {
                    self.done = true;
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "download stream ended early"));
                }
            }
        }
        Ok(())
    }
}

impl Read for FrameReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.remaining == 0 {
                self.finish()?;
                return Ok(0);
            }
            if self.pos < self.cur.len() {
                let k = out.len().min(self.cur.len() - self.pos).min(self.remaining as usize);
                out[..k].copy_from_slice(&self.cur[self.pos..self.pos + k]);
                self.pos += k;
                self.remaining -= k as u64;
                return Ok(k);
            }
            if self.next >= self.frames.len() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "download ended before the whole file arrived"));
            }
            match proto::read_frame(&mut self.remote.r, &mut self.remote.buf)? {
                Some(Kind::Data) => {}
                Some(Kind::Abort) => {
                    self.done = true;
                    return Err(io::Error::other(String::from_utf8_lossy(&self.remote.buf).into_owned()));
                }
                _ => {
                    self.done = true;
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "download stream ended early"));
                }
            }
            let d_len = self.frames[self.next] as usize;
            self.cur = zstd::bulk::decompress(&self.remote.buf, d_len).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            if self.cur.len() != d_len {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "frame decompressed to the wrong size"));
            }
            self.pos = if self.next == 0 { self.skip as usize } else { 0 };
            self.next += 1;
        }
    }
}

impl Drop for FrameReader<'_> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

impl Archive for Remote {
    fn stat(&mut self, path: &VPath) -> Result<Option<Entry>> {
        match self.call(&Request::Stat { path: path.clone() })? {
            Response::Entry(e) => Ok(e),
            other => Err(unexpected(other)),
        }
    }

    fn list(&mut self, path: &VPath) -> Result<Vec<Entry>> {
        match self.call(&Request::List { path: path.clone() })? {
            Response::Entries(e) => Ok(e),
            other => Err(unexpected(other)),
        }
    }

    fn walk(&mut self, path: &VPath) -> Result<Vec<(String, Entry)>> {
        match self.call(&Request::Walk { path: path.clone() })? {
            Response::Walk(e) => Ok(e),
            other => Err(unexpected(other)),
        }
    }

    fn sizes_present(&mut self, scope: &VPath, sizes: &[u64]) -> Result<Vec<bool>> {
        match self.call(&Request::SizesPresent { scope: scope.clone(), sizes: sizes.to_vec() })? {
            Response::Bools(b) => Ok(b),
            other => Err(unexpected(other)),
        }
    }

    fn have(&mut self, scope: &VPath, hashes: &[Digest]) -> Result<Vec<bool>> {
        match self.call(&Request::Have { scope: scope.clone(), hashes: hashes.to_vec() })? {
            Response::Bools(b) => Ok(b),
            other => Err(unexpected(other)),
        }
    }

    fn mkdirs(&mut self, job: &str, dirs: &[DirSpec]) -> Result<()> {
        for chunk in dirs.chunks(5000) {
            self.expect_ok(&Request::Mkdirs { job: job.to_string(), dirs: chunk.to_vec() })?;
        }
        Ok(())
    }

    fn symlink(&mut self, job: &str, dest: &VPath, target: &str, mtime_ns: i64, policy: Conflict) -> Result<PutOutcome> {
        let req = Request::Symlink { job: job.to_string(), dest: dest.clone(), target: target.to_string(), mtime_ns, policy };
        match self.call(&req)? {
            Response::Outcome(o) => Ok(o),
            other => Err(unexpected(other)),
        }
    }

    fn link(&mut self, job: &str, req: &PutFile, sha256: &Digest) -> Result<PutOutcome> {
        match self.call(&Request::Link { job: job.to_string(), req: req.clone(), sha256: *sha256 })? {
            Response::Outcome(o) => Ok(o),
            other => Err(unexpected(other)),
        }
    }

    fn link_many(&mut self, job: &str, items: &[(PutFile, Digest)]) -> Result<Vec<Result<PutOutcome>>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        match self.call(&Request::LinkMany { job: job.to_string(), items: items.to_vec() })? {
            Response::Results(rs) => Ok(rs.into_iter().map(proto::ItemResult::into_result).collect()),
            other => Err(unexpected(other)),
        }
    }

    fn put_file(&mut self, job: &str, req: &PutFile, payload: &mut dyn Payload) -> Result<PutOutcome> {
        match self.call(&Request::PutFile { job: job.to_string(), req: req.clone() })? {
            Response::Outcome(o) => return Ok(o),
            Response::Proceed => {}
            other => return Err(unexpected(other)),
        }
        self.stream(payload)?;
        match self.recv()? {
            Response::Outcome(o) => Ok(o),
            other => Err(unexpected(other)),
        }
    }

    fn put_solid(&mut self, job: &str, req: &PutSolid, payload: &mut dyn Payload) -> Result<Vec<PutOutcome>> {
        match self.call(&Request::PutSolid { job: job.to_string(), req: req.clone() })? {
            Response::Outcomes(o) => return Ok(o),
            Response::Proceed => {}
            other => return Err(unexpected(other)),
        }
        self.stream(payload)?;
        match self.recv()? {
            Response::Outcomes(o) => Ok(o),
            other => Err(unexpected(other)),
        }
    }

    fn settle(&mut self) -> Result<Vec<(VPath, String)>> {
        match self.call(&Request::Settle)? {
            Response::Failed(f) => Ok(f),
            other => Err(unexpected(other)),
        }
    }

    fn finish_job(&mut self, job: &str) -> Result<()> {
        self.expect_ok(&Request::FinishJob { job: job.to_string() })
    }

    fn read_file(&mut self, path: &VPath) -> Result<(Entry, Box<dyn Read + Send + '_>)> {
        match self.call(&Request::ReadFile { path: path.clone() })? {
            Response::FileStart { entry, frames, skip, len } => {
                let reader = FrameReader { remote: self, frames, next: 0, skip, remaining: len, cur: Vec::new(), pos: 0, done: false };
                Ok((entry, Box::new(reader)))
            }
            other => Err(unexpected(other)),
        }
    }

    fn read_files(&mut self, paths: &[VPath], each: &mut ReadEach<'_>) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        self.send(&Request::ReadMany { paths: paths.to_vec() })?;
        let mut stopped = false;
        for i in 0..paths.len() {
            match self.recv() {
                Ok(Response::FileStart { entry, frames, skip, len }) => {
                    let mut reader =
                        FrameReader { remote: self, frames, next: 0, skip, remaining: len, cur: Vec::new(), pos: 0, done: false };
                    if !stopped {
                        stopped = each(i, Ok((entry, &mut reader))).is_err();
                    }
                    // Dropping the reader consumes whatever the callback didn't read.
                    drop(reader);
                }
                Ok(other) => return Err(unexpected(other)),
                Err(e) => {
                    // A per-file error reply; a lost connection fails every later file too.
                    let fatal = e.is_lost();
                    if !stopped {
                        stopped = each(i, Err(e)).is_err();
                    }
                    if fatal {
                        return Err(Error::other("lost the connection to the server"));
                    }
                }
            }
        }
        Ok(())
    }

    fn info(&mut self, path: &VPath) -> Result<Info> {
        match self.call(&Request::Info { path: path.clone() })? {
            Response::Info(i) => Ok(i),
            other => Err(unexpected(other)),
        }
    }

    fn search(&mut self, query: &str, limit: usize) -> Result<Vec<(VPath, Entry)>> {
        match self.call(&Request::Search { query: query.to_string(), limit })? {
            Response::Search(s) => Ok(s),
            other => Err(unexpected(other)),
        }
    }

    fn create_folder(&mut self, path: &VPath) -> Result<()> {
        self.expect_ok(&Request::CreateFolder { path: path.clone() })
    }

    fn rename(&mut self, from: &VPath, to: &VPath) -> Result<()> {
        self.expect_ok(&Request::Rename { from: from.clone(), to: to.clone() })
    }

    fn trash(&mut self, path: &VPath) -> Result<i64> {
        match self.call(&Request::Trash { path: path.clone() })? {
            Response::Id(i) => Ok(i),
            other => Err(unexpected(other)),
        }
    }

    fn restore(&mut self, trash_id: i64, to: Option<&VPath>) -> Result<VPath> {
        match self.call(&Request::Restore { id: trash_id, to: to.cloned() })? {
            Response::Path(p) => Ok(p),
            other => Err(unexpected(other)),
        }
    }

    fn trash_list(&mut self) -> Result<Vec<Entry>> {
        match self.call(&Request::TrashList)? {
            Response::Entries(e) => Ok(e),
            other => Err(unexpected(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_targets() {
        let t = SshTarget::parse("ssh://lab-storage/mnt/pool/lab/archive").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.root.as_str()), ("lab-storage", None, "/mnt/pool/lab/archive"));
        let t = SshTarget::parse("ssh://alex@host:2222/~/archive").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.root.as_str()), ("alex@host", Some(2222), "~/archive"));
        assert!(SshTarget::parse("/local/path").is_none());
        assert!(SshTarget::parse("ssh:///nohost").is_none());
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
