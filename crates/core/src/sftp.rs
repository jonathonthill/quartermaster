//! SFTP servers: machines that offer only SFTP (no `archive-helper`). The app speaks SFTP version 3
//! itself, over the system `ssh` (`ssh -s host sftp`), so sign-in works exactly as for other servers:
//! `~/.ssh/config`, shared connections, and password or Duo questions in the app.
//!
//! An SFTP server can't compute checksums, so a copy to or from one is checked by size and date
//! (and, if the server's "read back" setting is on, by reading uploads back and comparing their
//! SHA-256). Reads and writes are pipelined: many requests are kept in flight, so a long round trip
//! doesn't make large files slow.
//!
//! Besides copying ([`crate::copy::Endpoint`]), this module browses (listing, places, measuring,
//! searching), makes folders, deletes, and keeps a Trash in the same layout the helper uses
//! (see [`crate::trash`]), moving items into it by rename.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::copy::{Endpoint, FileKind, FileStat, Walk, WalkItem};
use crate::error::{Error, Result};
use crate::fsview::{Crumb, Item, Listing, Measure, Place};
use crate::hash::{Digest, Hasher};
use crate::remote::SshSetup;
use crate::trash::{Failure, Report, Trashed};
use crate::util;

// Packet types.
const INIT: u8 = 1;
const VERSION: u8 = 2;
const OPEN: u8 = 3;
const CLOSE: u8 = 4;
const READ: u8 = 5;
const WRITE: u8 = 6;
const LSTAT: u8 = 7;
const SETSTAT: u8 = 9;
const OPENDIR: u8 = 11;
const READDIR: u8 = 12;
const REMOVE: u8 = 13;
const MKDIR: u8 = 14;
const RMDIR: u8 = 15;
const REALPATH: u8 = 16;
const STAT: u8 = 17;
const RENAME: u8 = 18;
const EXTENDED: u8 = 200;
const STATUS: u8 = 101;
const HANDLE: u8 = 102;
const DATA: u8 = 103;
const NAME: u8 = 104;
const ATTRS: u8 = 105;

// Open flags.
const F_READ: u32 = 1;
const F_WRITE: u32 = 2;
const F_CREAT: u32 = 8;
const F_TRUNC: u32 = 0x10;

// Attribute flags.
const A_SIZE: u32 = 1;
const A_UIDGID: u32 = 2;
const A_PERMS: u32 = 4;
const A_TIMES: u32 = 8;
const A_EXTENDED: u32 = 0x8000_0000;

// Status codes.
const FX_OK: u32 = 0;
const FX_EOF: u32 = 1;
const FX_NO_SUCH_FILE: u32 = 2;
const FX_PERMISSION_DENIED: u32 = 3;

/// Bytes per read or write request, and how many may be in flight at once.
const CHUNK: u32 = 32 * 1024;
const WINDOW: usize = 64;

/// What the server says about a path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attrs {
    pub size: Option<u64>,
    pub perms: Option<u32>,
    pub mtime: Option<u32>,
}

impl Attrs {
    pub fn kind(&self) -> FileKind {
        match self.perms.map(|p| p & 0o170000) {
            Some(0o040000) => FileKind::Dir,
            Some(0o120000) => FileKind::Link,
            _ => FileKind::File,
        }
    }

    fn stat(&self) -> FileStat {
        let kind = self.kind();
        FileStat {
            kind,
            size: if kind == FileKind::File { self.size.unwrap_or(0) } else { 0 },
            mtime_ns: self.mtime.unwrap_or(0) as i64 * 1_000_000_000,
            ctime_ns: 0,
            inode: 0,
            mode: self.perms.unwrap_or(0) & 0o7777,
        }
    }
}

/// An SFTP session.
pub struct Sftp {
    r: BufReader<Box<dyn Read + Send>>,
    w: BufWriter<Box<dyn Write + Send>>,
    child: Option<Child>,
    stderr: Arc<Mutex<String>>,
    next_id: u32,
    posix_rename: bool,
    home: String,
    host: String,
    /// Read uploads back and compare their checksum (off: check size only).
    pub read_back: bool,
    writing: Option<Upload>,
}

struct Upload {
    handle: Vec<u8>,
    part: String,
    target: String,
    size: u64,
    mtime_ns: i64,
    mode: u32,
    offset: u64,
    hasher: Hasher,
    outstanding: VecDeque<u32>,
}

/// Build packets.
struct Buf(Vec<u8>);

impl Buf {
    fn new(ty: u8) -> Buf {
        Buf(vec![0, 0, 0, 0, ty])
    }
    fn u32(mut self, v: u32) -> Buf {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u64(mut self, v: u64) -> Buf {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn bytes(mut self, v: &[u8]) -> Buf {
        self = self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
        self
    }
    fn str(self, v: &str) -> Buf {
        self.bytes(v.as_bytes())
    }
    fn attrs(self, a: &Attrs) -> Buf {
        let mut flags = 0;
        if a.size.is_some() {
            flags |= A_SIZE;
        }
        if a.perms.is_some() {
            flags |= A_PERMS;
        }
        if a.mtime.is_some() {
            flags |= A_TIMES;
        }
        let mut b = self.u32(flags);
        if let Some(s) = a.size {
            b = b.u64(s);
        }
        if let Some(p) = a.perms {
            b = b.u32(p);
        }
        if let Some(m) = a.mtime {
            b = b.u32(m).u32(m);
        }
        b
    }
    fn done(mut self) -> Vec<u8> {
        let len = (self.0.len() - 4) as u32;
        self.0[..4].copy_from_slice(&len.to_be_bytes());
        self.0
    }
}

/// Read packets.
struct Rd<'a>(&'a [u8]);

impl Rd<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.0.len() < n {
            return Err(Error::other("protocol error: a short SFTP reply"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    fn string(&mut self) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.bytes()?).into_owned())
    }
    fn attrs(&mut self) -> Result<Attrs> {
        let flags = self.u32()?;
        let mut a = Attrs::default();
        if flags & A_SIZE != 0 {
            a.size = Some(self.u64()?);
        }
        if flags & A_UIDGID != 0 {
            self.u32()?;
            self.u32()?;
        }
        if flags & A_PERMS != 0 {
            a.perms = Some(self.u32()?);
        }
        if flags & A_TIMES != 0 {
            self.u32()?;
            a.mtime = Some(self.u32()?);
        }
        if flags & A_EXTENDED != 0 {
            for _ in 0..self.u32()? {
                self.bytes()?;
                self.bytes()?;
            }
        }
        Ok(a)
    }
}

fn status_error(code: u32, msg: &str, path: &str) -> Error {
    match code {
        FX_NO_SUCH_FILE => Error::NotFound(path.to_string()),
        FX_PERMISSION_DENIED => Error::other(format!("{path}: permission denied")),
        _ if msg.is_empty() => Error::other(format!("{path}: the server refused (code {code})")),
        _ => Error::other(format!("{path}: {msg}")),
    }
}

fn parent_of(path: &str) -> &str {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => ".",
    }
}

fn name_of(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

impl Sftp {
    /// Start an SFTP session to `host` through the system ssh.
    pub fn ssh(host: &str, port: Option<u16>, setup: &SshSetup) -> Result<Sftp> {
        let mut cmd = setup.command();
        cmd.args(["-T", "-o", "ServerAliveInterval=30", "-o", "ServerAliveCountMax=4"]);
        if let Some(p) = port {
            cmd.args(["-p", &p.to_string()]);
        }
        cmd.arg("-s").arg(host).arg("sftp");
        Self::spawn(cmd, host)
    }

    /// Speak SFTP to a program's stdin and stdout (an `sftp-server`, for tests).
    pub fn spawn(mut cmd: Command, host: &str) -> Result<Sftp> {
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
        let mut s = Sftp {
            r: BufReader::with_capacity(1 << 20, Box::new(stdout)),
            w: BufWriter::with_capacity(1 << 20, Box::new(stdin)),
            child: Some(child),
            stderr,
            next_id: 1,
            posix_rename: false,
            home: String::new(),
            host: host.to_string(),
            read_back: false,
            writing: None,
        };
        s.init()?;
        Ok(s)
    }

    fn init(&mut self) -> Result<()> {
        let pkt = Buf::new(INIT).u32(3).done();
        self.send(&pkt)?;
        let (ty, body) = self.recv_raw()?;
        if ty != VERSION {
            return Err(Error::other("this server didn't start SFTP (is SFTP turned on there?)"));
        }
        let mut r = Rd(&body);
        r.u32()?;
        while !r.0.is_empty() {
            let name = r.string()?;
            r.bytes()?;
            if name == "posix-rename@openssh.com" {
                self.posix_rename = true;
            }
        }
        self.home = self.realpath(".")?;
        Ok(())
    }

    /// The home folder on the server.
    pub fn home(&self) -> &str {
        &self.home
    }

    fn lost(&self, e: io::Error) -> Error {
        let tail = self.stderr.lock().map(|s| s.trim().to_string()).unwrap_or_default();
        if tail.is_empty() {
            Error::other(format!("lost the connection to the server: {e}"))
        } else {
            Error::other(format!("lost the connection to the server: {tail}"))
        }
    }

    fn send(&mut self, pkt: &[u8]) -> Result<()> {
        self.w.write_all(pkt).map_err(|e| self.lost(e))
    }

    fn flush(&mut self) -> Result<()> {
        self.w.flush().map_err(|e| self.lost(e))
    }

    fn recv_raw(&mut self) -> Result<(u8, Vec<u8>)> {
        self.flush()?;
        let mut head = [0u8; 4];
        self.r.read_exact(&mut head).map_err(|e| self.lost(e))?;
        let len = u32::from_be_bytes(head) as usize;
        if len == 0 || len > 1 << 24 {
            return Err(Error::other("protocol error: a bad SFTP packet"));
        }
        let mut body = vec![0u8; len];
        self.r.read_exact(&mut body).map_err(|e| self.lost(e))?;
        let ty = body[0];
        body.remove(0);
        Ok((ty, body))
    }

    /// A reply: its type, request id, and the rest.
    fn recv(&mut self) -> Result<(u8, u32, Vec<u8>)> {
        let (ty, body) = self.recv_raw()?;
        let mut r = Rd(&body);
        let id = r.u32()?;
        Ok((ty, id, r.0.to_vec()))
    }

    fn id(&mut self) -> u32 {
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.next_id
    }

    /// Send one request and wait for its reply.
    fn call(&mut self, ty: u8, f: impl FnOnce(Buf) -> Buf) -> Result<(u8, Vec<u8>)> {
        let id = self.id();
        let pkt = f(Buf::new(ty).u32(id)).done();
        self.send(&pkt)?;
        loop {
            let (rty, rid, body) = self.recv()?;
            if rid == id {
                return Ok((rty, body));
            }
        }
    }

    fn expect_ok(&mut self, ty: u8, path: &str, f: impl FnOnce(Buf) -> Buf) -> Result<()> {
        let (rty, body) = self.call(ty, f)?;
        check_status(rty, &body, path)
    }

    pub fn realpath(&mut self, path: &str) -> Result<String> {
        let (ty, body) = self.call(REALPATH, |b| b.str(path))?;
        if ty != NAME {
            check_status(ty, &body, path)?;
            return Err(Error::other("protocol error: no path from the server"));
        }
        let mut r = Rd(&body);
        r.u32()?;
        r.string()
    }

    fn stat_op(&mut self, op: u8, path: &str) -> Result<Option<Attrs>> {
        let (ty, body) = self.call(op, |b| b.str(path))?;
        match ty {
            ATTRS => Ok(Some(Rd(&body).attrs()?)),
            STATUS => {
                let mut r = Rd(&body);
                let code = r.u32()?;
                if code == FX_NO_SUCH_FILE { Ok(None) } else { Err(status_error(code, &r.string().unwrap_or_default(), path)) }
            }
            _ => Err(Error::other("protocol error: unexpected SFTP reply")),
        }
    }

    /// The path itself (a link is not followed).
    pub fn lstat(&mut self, path: &str) -> Result<Option<Attrs>> {
        self.stat_op(LSTAT, path)
    }

    /// What a path leads to (links are followed).
    pub fn stat_follow(&mut self, path: &str) -> Result<Option<Attrs>> {
        self.stat_op(STAT, path)
    }

    /// The names in a folder (without `.` and `..`).
    pub fn read_dir(&mut self, path: &str) -> Result<Vec<(String, Attrs)>> {
        let (ty, body) = self.call(OPENDIR, |b| b.str(path))?;
        let handle = self.handle(ty, &body, path)?;
        let mut out = Vec::new();
        loop {
            let (ty, body) = self.call(READDIR, |b| b.bytes(&handle))?;
            if ty == STATUS {
                let code = Rd(&body).u32()?;
                if code == FX_EOF {
                    break;
                }
                let _ = self.close(&handle);
                check_status(ty, &body, path)?;
            }
            let mut r = Rd(&body);
            for _ in 0..r.u32()? {
                let name = r.string()?;
                r.string()?;
                let a = r.attrs()?;
                if name != "." && name != ".." {
                    out.push((name, a));
                }
            }
        }
        self.close(&handle)?;
        Ok(out)
    }

    fn handle(&self, ty: u8, body: &[u8], path: &str) -> Result<Vec<u8>> {
        if ty == HANDLE {
            return Rd(body).bytes();
        }
        check_status(ty, body, path)?;
        Err(Error::other("protocol error: no handle from the server"))
    }

    fn close(&mut self, handle: &[u8]) -> Result<()> {
        self.expect_ok(CLOSE, "", |b| b.bytes(handle))
    }

    pub fn mkdir(&mut self, path: &str) -> Result<()> {
        self.expect_ok(MKDIR, path, |b| b.str(path).attrs(&Attrs::default()))
    }

    /// Make a folder and any missing parents; folders that exist are fine.
    pub fn mkdir_all(&mut self, path: &str) -> Result<()> {
        match self.lstat(path)? {
            Some(a) if a.kind() == FileKind::Dir => return Ok(()),
            Some(_) => return Err(Error::NotADirectory(path.to_string())),
            None => {}
        }
        let parent = parent_of(path);
        if parent != path && parent != "." {
            self.mkdir_all(parent)?;
        }
        self.mkdir(path)
    }

    pub fn remove_file(&mut self, path: &str) -> Result<()> {
        self.expect_ok(REMOVE, path, |b| b.str(path))
    }

    pub fn rmdir(&mut self, path: &str) -> Result<()> {
        self.expect_ok(RMDIR, path, |b| b.str(path))
    }

    /// Rename, replacing what's at `to` if the server allows (OpenSSH does).
    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        if self.posix_rename {
            return self.expect_ok(EXTENDED, from, |b| b.str("posix-rename@openssh.com").str(from).str(to));
        }
        if self.lstat(to)?.is_some() {
            self.remove_file(to)?;
        }
        self.expect_ok(RENAME, from, |b| b.str(from).str(to))
    }

    /// Rename only if nothing is at `to` (for the Trash, which must never overwrite).
    fn rename_new(&mut self, from: &str, to: &str) -> Result<()> {
        if self.lstat(to)?.is_some() {
            return Err(Error::AlreadyExists(to.to_string()));
        }
        self.expect_ok(RENAME, from, |b| b.str(from).str(to))
    }

    pub fn set_times_mode(&mut self, path: &str, mtime_ns: i64, mode: u32) -> Result<()> {
        let a = Attrs {
            size: None,
            perms: (mode != 0).then_some(mode & 0o7777),
            mtime: Some((mtime_ns.div_euclid(1_000_000_000)).max(0) as u32),
        };
        self.expect_ok(SETSTAT, path, |b| b.str(path).attrs(&a))
    }

    fn open(&mut self, path: &str, flags: u32) -> Result<Vec<u8>> {
        let (ty, body) = self.call(OPEN, |b| b.str(path).u32(flags).attrs(&Attrs::default()))?;
        self.handle(ty, &body, path)
    }

    /// Read a file from `start` to its end, handing each piece to `sink` in order. Many reads are
    /// kept in flight at once.
    pub fn read_from(&mut self, path: &str, start: u64, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let handle = self.open(path, F_READ)?;
        let result = self.read_pipelined(&handle, start, sink);
        let closed = self.close(&handle);
        result.and(closed)
    }

    fn read_pipelined(&mut self, handle: &[u8], start: u64, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        enum Got {
            Data(Vec<u8>),
            Eof,
        }
        let mut queue: VecDeque<(u32, u64, u32)> = VecDeque::new();
        let mut got: HashMap<u32, Got> = HashMap::new();
        let mut next = start;
        let mut eof = false;
        let mut failed: Option<Error> = None;
        loop {
            while !eof && failed.is_none() && queue.len() < WINDOW {
                let id = self.id();
                let pkt = Buf::new(READ).u32(id).bytes(handle).u64(next).u32(CHUNK).done();
                self.send(&pkt)?;
                queue.push_back((id, next, CHUNK));
                next += CHUNK as u64;
            }
            if queue.is_empty() {
                break;
            }
            let (ty, id, body) = self.recv()?;
            match ty {
                DATA => {
                    got.insert(id, Got::Data(Rd(&body).bytes()?));
                }
                STATUS => {
                    let mut r = Rd(&body);
                    let code = r.u32()?;
                    if code == FX_EOF {
                        got.insert(id, Got::Eof);
                    } else {
                        got.insert(id, Got::Eof);
                        failed.get_or_insert(status_error(code, &r.string().unwrap_or_default(), "the file"));
                    }
                }
                _ => return Err(Error::other("protocol error: unexpected SFTP reply")),
            }
            // Hand over, in order, whatever has arrived.
            while let Some(&(fid, off, len)) = queue.front() {
                let Some(g) = got.remove(&fid) else { break };
                queue.pop_front();
                match g {
                    Got::Eof => eof = true,
                    Got::Data(_) if eof || failed.is_some() => {}
                    Got::Data(d) => {
                        if let Err(e) = sink(&d) {
                            failed.get_or_insert(e);
                        }
                        // A short read before the end: fetch the rest of that piece next.
                        if (d.len() as u32) < len && !d.is_empty() {
                            let rest = off + d.len() as u64;
                            let id = self.id();
                            let pkt = Buf::new(READ).u32(id).bytes(handle).u64(rest).u32(len - d.len() as u32).done();
                            self.send(&pkt)?;
                            queue.push_front((id, rest, len - d.len() as u32));
                        } else if d.is_empty() {
                            eof = true;
                        }
                    }
                }
            }
        }
        match failed {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn write_chunk(&mut self, data: &[u8]) -> Result<()> {
        let up = self.writing.as_mut().ok_or_else(|| Error::other("no file is being written"))?;
        let id = {
            self.next_id = self.next_id.wrapping_add(1).max(1);
            self.next_id
        };
        let pkt = Buf::new(WRITE).u32(id).bytes(&up.handle).u64(up.offset).bytes(data).done();
        up.offset += data.len() as u64;
        up.hasher.update(data);
        up.outstanding.push_back(id);
        self.send(&pkt)?;
        while self.writing.as_ref().is_some_and(|u| u.outstanding.len() >= WINDOW) {
            self.write_reply()?;
        }
        Ok(())
    }

    fn write_reply(&mut self) -> Result<()> {
        let (ty, id, body) = self.recv()?;
        let target = self.writing.as_ref().map(|u| u.target.clone()).unwrap_or_default();
        if let Some(up) = self.writing.as_mut() {
            up.outstanding.retain(|x| *x != id);
        }
        check_status(ty, &body, &target)
    }

    /// Delete a file or folder and everything in it. Links are removed, not followed.
    pub fn remove_tree(&mut self, path: &str) -> Result<()> {
        let Some(a) = self.lstat(path)? else { return Ok(()) };
        if a.kind() != FileKind::Dir {
            return self.remove_file(path);
        }
        for (name, _) in self.read_dir(path)? {
            self.remove_tree(&join(path, &name))?;
        }
        self.rmdir(path)
    }

    // ------------------------------------------------------------ browsing

    /// A folder listing, shaped like a file server's.
    pub fn list(&mut self, path: Option<&str>, show_hidden: bool) -> Result<Listing> {
        let dir = match path {
            None => self.home.clone(),
            Some(p) => self.typed(p)?,
        };
        match self.stat_follow(&dir)? {
            Some(a) if a.kind() == FileKind::Dir => {}
            Some(_) => return Err(Error::NotADirectory(dir)),
            None => return Err(Error::NotFound(dir)),
        }
        let mut items = Vec::new();
        for (name, a) in self.read_dir(&dir)? {
            if crate::send::SYSTEM_JUNK.iter().any(|p| crate::send::glob_match(p, &name)) || (!show_hidden && name.starts_with('.')) {
                continue;
            }
            let full = join(&dir, &name);
            let mut kind = a.kind();
            // Follow links to folders so they can be opened.
            if kind == FileKind::Link && self.stat_follow(&full).ok().flatten().is_some_and(|t| t.kind() == FileKind::Dir) {
                kind = FileKind::Dir;
            }
            items.push(Item {
                name,
                path: full,
                kind: match kind {
                    FileKind::Dir => "dir",
                    FileKind::Link => "link",
                    FileKind::File => "file",
                }
                .into(),
                size: (kind == FileKind::File).then_some(a.size.unwrap_or(0)),
                files: None,
                mtime: a.mtime.unwrap_or(0) as i64,
                archived: None,
                is_project: false,
                in_project: false,
            });
        }
        crate::fsview::sort(&mut items);
        Ok(Listing {
            parent: (dir != "/").then(|| parent_of(&dir).to_string()),
            crumbs: self.crumbs(&dir),
            path: dir,
            items,
            free_bytes: None,
            project: None,
        })
    }

    /// A path as typed: `~` is the home folder, `.` and `..` are resolved.
    pub fn typed(&self, path: &str) -> Result<String> {
        let path = path.trim();
        let full = if path == "~" {
            self.home.clone()
        } else if let Some(rest) = path.strip_prefix("~/") {
            join(&self.home, rest)
        } else if path.starts_with('/') {
            path.to_string()
        } else {
            return Err(Error::InvalidPath(format!("Type a full path, starting with / or ~ (not “{path}”).")));
        };
        let mut parts: Vec<&str> = Vec::new();
        for c in full.split('/') {
            match c {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                other => parts.push(other),
            }
        }
        Ok(format!("/{}", parts.join("/")))
    }

    fn crumbs(&self, dir: &str) -> Vec<Crumb> {
        let mut out = vec![Crumb { name: "/".into(), path: "/".into() }];
        let mut cur = String::new();
        for part in dir.split('/').filter(|p| !p.is_empty()) {
            cur = format!("{cur}/{part}");
            out.push(Crumb { name: part.to_string(), path: cur.clone() });
        }
        // Paths under the home folder start from it, as on other servers.
        if let Some(i) = out.iter().position(|c| c.path == self.home) {
            out.drain(..i);
            if let Some(first) = out.first_mut() {
                first.name = "Home".into();
            }
        }
        out
    }

    pub fn places(&mut self) -> Vec<Place> {
        vec![Place { name: "Home".into(), path: self.home.clone() }, Place { name: "Top level (/)".into(), path: "/".into() }]
    }

    /// Count files and bytes under these paths.
    pub fn measure(&mut self, paths: &[String]) -> Result<Measure> {
        let mut m = Measure::default();
        let mut stack: Vec<String> = paths.to_vec();
        while let Some(p) = stack.pop() {
            let Some(a) = self.lstat(&p)? else { continue };
            match a.kind() {
                FileKind::Dir => {
                    m.folders += 1;
                    for (name, _) in self.read_dir(&p)? {
                        stack.push(join(&p, &name));
                    }
                }
                FileKind::File => {
                    m.files += 1;
                    m.bytes += a.size.unwrap_or(0);
                }
                FileKind::Link => {}
            }
        }
        Ok(m)
    }

    /// Names containing `query` under `root`, folder by folder (an SFTP server can't search for us),
    /// stopping at `limit` matches or when `stop` says so.
    pub fn search(
        &mut self,
        root: &str,
        query: &str,
        show_hidden: bool,
        limit: usize,
        stop: &dyn Fn() -> bool,
    ) -> Result<crate::fsview::SearchResult> {
        let q = query.to_lowercase();
        let mut hits = Vec::new();
        let mut scanned = 0u64;
        let mut queue: VecDeque<String> = VecDeque::from([root.to_string()]);
        let start = std::time::Instant::now();
        let mut timed_out = false;
        while let Some(dir) = queue.pop_front() {
            if stop() || start.elapsed() > std::time::Duration::from_secs(20) {
                timed_out = !stop();
                break;
            }
            let Ok(entries) = self.read_dir(&dir) else { continue };
            for (name, a) in entries {
                if !show_hidden && name.starts_with('.') {
                    continue;
                }
                scanned += 1;
                let full = join(&dir, &name);
                if a.kind() == FileKind::Dir {
                    queue.push_back(full.clone());
                }
                if name.to_lowercase().contains(&q) {
                    let kind = match a.kind() {
                        FileKind::Dir => "dir",
                        FileKind::Link => "link",
                        FileKind::File => "file",
                    };
                    hits.push(crate::fsview::Hit {
                        folder: dir.clone(),
                        item: Item {
                            name,
                            path: full,
                            kind: kind.into(),
                            size: (kind == "file").then_some(a.size.unwrap_or(0)),
                            files: None,
                            mtime: a.mtime.unwrap_or(0) as i64,
                            archived: None,
                            is_project: false,
                            in_project: false,
                        },
                    });
                    if hits.len() >= limit {
                        return Ok(crate::fsview::SearchResult {
                            hits,
                            truncated: true,
                            timed_out: false,
                            scanned,
                            method: crate::findfiles::Method::Scan,
                        });
                    }
                }
            }
        }
        Ok(crate::fsview::SearchResult { hits, truncated: false, timed_out, scanned, method: crate::findfiles::Method::Scan })
    }

    /// Why `path` must never be trashed or deleted. (An SFTP server can't tell us about mounted
    /// disks, so only the top level and the home folder are protected here.)
    fn protected(&self, path: &str) -> Option<String> {
        let p = path.trim_end_matches('/');
        if p.is_empty() || parent_of(p) == "/" {
            return Some("that is the top level of the server, not something to delete".into());
        }
        if self.home == p || self.home.starts_with(&format!("{p}/")) {
            return Some("that holds your home folder".into());
        }
        None
    }

    /// Delete these items, with everything in them, for good. Everything is checked first.
    pub fn delete(&mut self, paths: &[String]) -> Result<usize> {
        for p in paths {
            if let Some(why) = self.protected(p) {
                return Err(Error::InvalidPath(format!("Not deleted: {p} — {why}.")));
            }
            if self.lstat(p)?.is_none() {
                return Err(Error::NotFound(format!("{p} isn't there any more.")));
            }
        }
        for p in paths {
            self.remove_tree(p)?;
        }
        Ok(paths.len())
    }

    // --------------------------------------------------------------- Trash

    fn trash_dir(&self) -> String {
        join(&self.home, ".local/share/archive-helper/trash/items")
    }

    fn write_small(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let handle = self.open(path, F_WRITE | F_CREAT | F_TRUNC)?;
        let result = self.expect_ok(WRITE, path, |b| b.bytes(&handle).u64(0).bytes(data));
        let closed = self.close(&handle);
        result.and(closed)
    }

    fn read_small(&mut self, path: &str) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.read_from(path, 0, &mut |d| {
            out.extend_from_slice(d);
            Ok(())
        })?;
        Ok(out)
    }

    /// Send items to the Trash on this server (the same layout the helper keeps), by renaming them
    /// into it. One that can't be moved there (another disk the user can't write to, say) is
    /// reported, and the rest still go.
    pub fn trash(&mut self, paths: &[String]) -> Result<Report> {
        let notes = self.trash_dir();
        for p in paths {
            if let Some(why) = self.protected(p) {
                return Err(Error::InvalidPath(format!("Not sent to the Trash: {p} — {why}.")));
            }
            if p.starts_with(&join(&self.home, ".local/share/archive-helper/trash")) || p.contains("/.qm-trash/") {
                return Err(Error::InvalidPath(format!("Not sent to the Trash: {p} — that is already in the Trash.")));
            }
            if self.lstat(p)?.is_none() {
                return Err(Error::NotFound(format!("{p} isn't there any more.")));
            }
        }
        self.mkdir_all(&notes)?;
        let mut report = Report::default();
        for p in paths {
            let a = self.lstat(p)?.unwrap_or_default();
            let name = name_of(p).to_string();
            let id = util::random_hex(6);
            let home_side = join(&notes, &id);
            let mut stored = join(&home_side, &name);
            let moved = self.mkdir(&home_side).and_then(|_| self.rename_new(p, &stored));
            if moved.is_err() {
                let _ = self.rmdir(&home_side);
                // Perhaps another disk: keep it there, beside where it was.
                let beside = join(&join(parent_of(p), ".qm-trash"), &id);
                stored = join(&beside, &name);
                let again = self.mkdir_all(&beside).and_then(|_| self.rename_new(p, &stored));
                if let Err(e) = again {
                    let _ = self.rmdir(&beside);
                    report.failed.push(Failure { path: p.clone(), reason: format!("a Trash can't be made there ({e})"), can_delete: true });
                    continue;
                }
            }
            let t = Trashed {
                id: id.clone(),
                name,
                original: p.clone(),
                stored: stored.clone(),
                kind: if a.kind() == FileKind::Dir { "dir" } else { "file" }.into(),
                size: if a.kind() == FileKind::Dir { 0 } else { a.size.unwrap_or(0) },
                trashed_at: util::now_secs(),
            };
            if let Err(e) = self.write_small(&join(&notes, &format!("{id}.json")), &serde_json::to_vec_pretty(&t)?) {
                let _ = self.rename_new(&stored, p);
                report.failed.push(Failure {
                    path: p.clone(),
                    reason: format!("the Trash couldn't keep a note of it ({e})"),
                    can_delete: true,
                });
                continue;
            }
            report.trashed.push(t);
        }
        Ok(report)
    }

    fn forget(&mut self, t: &Trashed) {
        let _ = self.remove_file(&join(&self.trash_dir(), &format!("{}.json", t.id)));
        let holder = parent_of(&t.stored).to_string();
        let _ = self.rmdir(&holder);
        let beside = parent_of(&holder).to_string();
        if name_of(&beside) == ".qm-trash" {
            let _ = self.rmdir(&beside);
        }
    }

    /// What is in the Trash, newest first.
    pub fn trash_list(&mut self) -> Result<Vec<Trashed>> {
        let notes = self.trash_dir();
        let entries = match self.read_dir(&notes) {
            Ok(e) => e,
            Err(Error::NotFound(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        for (name, _) in entries.into_iter().filter(|(n, _)| n.ends_with(".json")) {
            let Ok(bytes) = self.read_small(&join(&notes, &name)) else { continue };
            let Ok(t) = serde_json::from_slice::<Trashed>(&bytes) else { continue };
            if self.lstat(&t.stored)?.is_some() {
                out.push(t);
            } else {
                self.forget(&t);
            }
        }
        out.sort_by_key(|t| std::cmp::Reverse(t.trashed_at));
        Ok(out)
    }

    /// Put an item back where it was. Nothing is overwritten.
    pub fn trash_restore(&mut self, id: &str) -> Result<String> {
        let t = self
            .trash_list()?
            .into_iter()
            .find(|t| t.id == id)
            .ok_or_else(|| Error::NotFound("that item isn't in the Trash any more".into()))?;
        if self.lstat(&t.original)?.is_some() {
            return Err(Error::AlreadyExists(t.original.clone()));
        }
        if self.stat_follow(parent_of(&t.original))?.is_none() {
            return Err(Error::NotFound(format!("the folder it came from ({}) is gone", parent_of(&t.original))));
        }
        self.rename_new(&t.stored, &t.original)?;
        self.forget(&t);
        Ok(t.original)
    }

    /// Delete items in the Trash for good: these, or all of them. Returns how many.
    pub fn trash_empty(&mut self, ids: Option<&[String]>) -> Result<usize> {
        let mut n = 0;
        for t in self.trash_list()? {
            if ids.is_some_and(|ids| !ids.contains(&t.id)) {
                continue;
            }
            self.remove_tree(&t.stored)?;
            self.forget(&t);
            n += 1;
        }
        Ok(n)
    }
}

fn check_status(ty: u8, body: &[u8], path: &str) -> Result<()> {
    if ty != STATUS {
        return Err(Error::other("protocol error: unexpected SFTP reply"));
    }
    let mut r = Rd(body);
    let code = r.u32()?;
    if code == FX_OK {
        return Ok(());
    }
    Err(status_error(code, &r.string().unwrap_or_default(), path))
}

impl Drop for Sftp {
    fn drop(&mut self) {
        let _ = self.w.flush();
        if let Some(mut c) = self.child.take() {
            drop(std::mem::replace(&mut self.w, BufWriter::new(Box::new(io::sink()))));
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

// --------------------------------------------------------------- copying

impl Endpoint for Sftp {
    fn machine(&self) -> String {
        self.host.clone()
    }
    fn join(&self, dir: &str, name: &str) -> String {
        join(dir, name)
    }
    fn verifies_writes(&self) -> bool {
        self.read_back
    }
    fn verifies_reads(&self) -> bool {
        false
    }
    fn walk(&mut self, root: &str) -> Result<Walk> {
        let top = self.stat_follow(root)?.ok_or_else(|| Error::NotFound(root.to_string()))?;
        let name = name_of(root).to_string();
        let mut walk = Walk::default();
        let is_dir = top.kind() == FileKind::Dir;
        walk.items.push(WalkItem { rel: name.clone(), path: root.to_string(), stat: top.stat() });
        if is_dir {
            let mut stack = vec![(root.to_string(), name)];
            while let Some((dir, rel)) = stack.pop() {
                let mut entries = match self.read_dir(&dir) {
                    Ok(e) => e,
                    Err(e) if e.is_lost() => return Err(e),
                    Err(e) => {
                        walk.problems.push((dir.clone(), format!("Can't open the folder: {e}")));
                        continue;
                    }
                };
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                for (n, a) in entries {
                    let p = join(&dir, &n);
                    let r = format!("{rel}/{n}");
                    if a.kind() == FileKind::Dir {
                        stack.push((p.clone(), r.clone()));
                    }
                    walk.items.push(WalkItem { rel: r, path: p, stat: a.stat() });
                }
            }
            // Parents before their contents, as the copy expects.
            walk.items.sort_by(|a, b| a.rel.matches('/').count().cmp(&b.rel.matches('/').count()));
        }
        Ok(walk)
    }
    fn stat(&mut self, path: &str) -> Result<Option<FileStat>> {
        Ok(self.lstat(path)?.map(|a| a.stat()))
    }
    fn mkdirs(&mut self, dirs: &[String]) -> Result<()> {
        for d in dirs {
            self.mkdir_all(d)?;
        }
        Ok(())
    }
    fn read(&mut self, path: &str, offset: u64, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<Digest> {
        // The server can't hash the part before `offset`, so it is read too (hashed, not sent).
        let mut h = Hasher::new();
        let mut pos = 0u64;
        self.read_from(path, 0, &mut |d| {
            h.update(d);
            let end = pos + d.len() as u64;
            if end > offset {
                let skip = offset.saturating_sub(pos) as usize;
                sink(&d[skip..])?;
            }
            pos = end;
            Ok(())
        })?;
        Ok(h.finish())
    }
    fn begin_write(&mut self, path: &str, size: u64, mtime_ns: i64, mode: u32, offset: u64) -> Result<()> {
        let part = crate::copy::part_string(path);
        let mut hasher = Hasher::new();
        if offset > 0 {
            // Continue a part file: its length must match, and its bytes are hashed to check the whole.
            let have = self.lstat(&part)?.and_then(|a| a.size).unwrap_or(0);
            if have < offset {
                return Err(Error::other("the partial copy is shorter than expected"));
            }
            let mut pos = 0u64;
            self.read_from(&part, 0, &mut |d| {
                let take = (offset.saturating_sub(pos)).min(d.len() as u64) as usize;
                hasher.update(&d[..take]);
                pos += d.len() as u64;
                Ok(())
            })?;
        }
        let flags = F_WRITE | F_CREAT | if offset == 0 { F_TRUNC } else { 0 };
        let handle = self.open(&part, flags)?;
        self.writing =
            Some(Upload { handle, part, target: path.to_string(), size, mtime_ns, mode, offset, hasher, outstanding: VecDeque::new() });
        Ok(())
    }
    fn write(&mut self, data: &[u8]) -> Result<()> {
        for piece in data.chunks(CHUNK as usize) {
            self.write_chunk(piece)?;
        }
        Ok(())
    }
    fn finish_write(&mut self, digest: Digest) -> Result<()> {
        while self.writing.as_ref().is_some_and(|u| !u.outstanding.is_empty()) {
            self.write_reply()?;
        }
        let up = self.writing.take().ok_or_else(|| Error::other("no file is being written"))?;
        self.close(&up.handle)?;
        let name = name_of(&up.target).to_string();
        let mismatch = |s: &mut Sftp| {
            let _ = s.remove_file(&up.part);
            Error::Verify(format!("{name}: the copy on the server doesn't match"))
        };
        if up.offset != up.size || up.hasher.finish() != digest {
            return Err(mismatch(self));
        }
        // What the server can tell us: the size it stored.
        if self.lstat(&up.part)?.and_then(|a| a.size) != Some(up.size) {
            return Err(mismatch(self));
        }
        if self.read_back {
            let mut h = Hasher::new();
            self.read_from(&up.part, 0, &mut |d| {
                h.update(d);
                Ok(())
            })?;
            if h.finish() != digest {
                return Err(mismatch(self));
            }
        }
        self.set_times_mode(&up.part, up.mtime_ns, up.mode)?;
        self.rename(&up.part, &up.target)
    }
    fn abort_write(&mut self) {
        // Keep the part file, so the copy can continue from it.
        if let Some(up) = self.writing.take() {
            let mut outstanding = up.outstanding.len();
            while outstanding > 0 && self.recv().is_ok() {
                outstanding -= 1;
            }
            let _ = self.close(&up.handle);
        }
    }
    fn remove(&mut self, path: &str) -> Result<()> {
        match self.lstat(path)? {
            Some(a) if a.kind() == FileKind::Dir => self.rmdir(path),
            Some(_) => self.remove_file(path),
            None => Ok(()),
        }
    }
}
