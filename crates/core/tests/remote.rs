//! The protocol end to end: a server thread running `serve` against a real
//! store, and a `Remote` client connected to it through OS pipes.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

mod common;
use common::*;

use archive_core::api::{Archive, DirSpec, Payload, PutFile};
use archive_core::catalog::{self, PackState};
use archive_core::hash::Digest;
use archive_core::proto::InitOptions;
use archive_core::remote::Remote;
use archive_core::retrieve::{self, RetrieveOptions};
use archive_core::seekable::{CompressOptions, CompressingReader};
use archive_core::send::{self, Mode, SendOptions};
use archive_core::serve::{self, ServeOptions};
use archive_core::store::{Config, Store};
use archive_core::worker::{self, WorkerOptions};
use archive_core::{Conflict, Error, FileMeta, VPath};

struct Server {
    client: Option<Remote>,
    thread: Option<JoinHandle<archive_core::Result<()>>>,
}

impl Server {
    fn remote(&mut self) -> &mut Remote {
        self.client.as_mut().unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        drop(self.client.take()); // closes the pipes, ending the server loop
        if let Some(t) = self.thread.take() {
            t.join().unwrap().unwrap();
        }
    }
}

fn start(root: &Path, restricted: bool) -> Server {
    let (c2s_r, c2s_w) = io::pipe().unwrap();
    let (s2c_r, s2c_w) = io::pipe().unwrap();
    let opts = ServeOptions { root: root.to_path_buf(), restricted, helper_version: "test".into(), kick_worker: None };
    let thread = std::thread::spawn(move || serve::serve(c2s_r, s2c_w, opts));
    let client = Remote::connect(Box::new(s2c_r), Box::new(c2s_w)).unwrap();
    Server { client: Some(client), thread: Some(thread) }
}

fn opts(mode: Mode) -> SendOptions {
    SendOptions {
        mode,
        compress: CompressOptions { level: 3, frame_size: 64 << 10, threads: 2 },
        solid_block_max: 256 << 10,
        ..SendOptions::default()
    }
}

fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("project");
    make_tree(&src);
    let root = tmp.path().join("arc");
    (tmp, src, root)
}

#[test]
fn init_send_retrieve_over_the_wire() {
    let (tmp, src, root) = setup();
    let before = snapshot(&src);
    let mut s = start(&root, false);
    assert!(s.remote().hello().archive_id.is_none());
    assert!(s.remote().stat(&VPath::root()).is_err(), "no archive yet");
    s.remote().init(InitOptions { pack_target_bytes: Some(1 << 20), par2_redundancy_percent: None, trash_days: None }).unwrap();
    assert!(s.remote().hello().archive_id.is_some());

    let r = send::send(s.remote(), &[src.clone()], &VPath::parse("/Projects").unwrap(), &opts(Mode::Move), &mut |_| {}).unwrap();
    assert!(r.ok(), "{r:?}");
    assert!(r.deduplicated >= 1);
    assert!(!src.exists(), "Move over the wire removes the source");

    let out = tmp.path().join("out");
    let g = retrieve::retrieve(s.remote(), &VPath::parse("/Projects/project").unwrap(), &out, &RetrieveOptions::default(), &mut |_| {})
        .unwrap();
    assert!(g.ok(), "{:?}", g.failed);
    assert_same(&snapshot(&out.join("project")), &before);

    // Browsing and editing calls.
    let info = s.remote().info(&VPath::parse("/Projects/project").unwrap()).unwrap();
    assert_eq!(info.original_bytes, r.bytes);
    assert!(!s.remote().search("résumé", 10).unwrap().is_empty());
    s.remote().create_folder(&VPath::parse("/Old").unwrap()).unwrap();
    s.remote().rename(&VPath::parse("/Projects/project").unwrap(), &VPath::parse("/Old/project").unwrap()).unwrap();
    let id = s.remote().trash(&VPath::parse("/Old/project").unwrap()).unwrap();
    assert_eq!(s.remote().trash_list().unwrap().len(), 1);
    assert_eq!(s.remote().restore(id, None).unwrap().to_string(), "/Old/project");
    assert!(!s.remote().packs().unwrap().is_empty());
}

/// A payload that fails partway through, like a disk read error.
struct Failing<R: Read> {
    inner: R,
    left: usize,
}

impl<R: Read> Read for Failing<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Err(io::Error::other("simulated read error"));
        }
        let n = buf.len().min(self.left);
        let k = self.inner.read(&mut buf[..n])?;
        self.left -= k;
        Ok(k)
    }
}

impl<R: Read> Payload for Failing<R> {
    fn digest(&self) -> Option<Digest> {
        None
    }
}

/// Flips one byte of an otherwise good payload.
struct Flip<R: Read> {
    inner: CompressingReader<R>,
    pos: u64,
}

impl<R: Read> Read for Flip<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if self.pos <= 200 && 200 < self.pos + n as u64 {
            buf[(200 - self.pos) as usize] ^= 1;
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read> Payload for Flip<R> {
    fn digest(&self) -> Option<Digest> {
        self.inner.digest()
    }
}

#[test]
fn aborted_and_corrupted_uploads_keep_the_connection_usable() {
    let (_tmp, src, root) = setup();
    Store::init(&root, Config::default()).unwrap();
    let mut s = start(&root, false);
    let data = fs::read(src.join("sub/big_text.fastq")).unwrap();
    let req = PutFile {
        dest: VPath::parse("/x/big.fastq").unwrap(),
        meta: FileMeta { size: data.len() as u64, mtime_ns: 0, mode: 0o644 },
        policy: Conflict::Skip,
    };
    let copts = CompressOptions { level: 3, frame_size: 64 << 10, threads: 2 };
    let top = DirSpec { path: VPath::parse("/x").unwrap(), mtime_ns: 0, mode: 0o755, project: true };
    s.remote().mkdirs("j", &[top]).unwrap();

    // Reader error mid-stream: aborted, nothing stored, connection still in step.
    let mut bad = Failing { inner: CompressingReader::new(&data[..], copts).unwrap(), left: 100_000 };
    assert!(s.remote().put_file("j", &req, &mut bad).is_err());
    assert!(s.remote().stat(&req.dest).unwrap().is_none());

    // Corrupted in transit: the server checks it in the background while more
    // could arrive, then reports it as failed and doesn't keep it.
    let mut flip = Flip { inner: CompressingReader::new(&data[..], copts).unwrap(), pos: 0 };
    match s.remote().put_file("j", &req, &mut flip) {
        Ok(_) | Err(Error::Verify(_)) | Err(Error::Corrupt(_)) => {}
        other => panic!("expected the upload to be accepted or rejected, got {other:?}"),
    }
    let failed = s.remote().settle().unwrap();
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].0, req.dest);
    assert!(s.remote().stat(&req.dest).unwrap().is_none());
    assert!(s.remote().settle().unwrap().is_empty(), "reported once");

    // A clean upload afterwards succeeds.
    let mut good = CompressingReader::new(&data[..], copts).unwrap();
    s.remote().put_file("j", &req, &mut good).unwrap();
    s.remote().finish_job("j").unwrap();
    let mut back = Vec::new();
    s.remote().read_file(&req.dest).unwrap().1.read_to_end(&mut back).unwrap();
    assert_eq!(back, data);

    // Abandoning a download partway leaves the connection in step too.
    {
        let (_, mut rd) = s.remote().read_file(&req.dest).unwrap();
        let mut first = [0u8; 10];
        rd.read_exact(&mut first).unwrap();
    }
    assert!(s.remote().stat(&req.dest).unwrap().is_some());
}

#[test]
fn restricted_connections_cannot_delete_or_rename() {
    let (_tmp, src, root) = setup();
    let mut store = Store::init(&root, Config::default()).unwrap();
    send::send(&mut store, &[src.clone()], &VPath::root(), &opts(Mode::Copy), &mut |_| {}).unwrap();
    drop(store);
    let mut s = start(&root, true);
    assert!(s.remote().hello().restricted);
    assert!(s.remote().stat(&VPath::parse("/project/small.csv").unwrap()).unwrap().is_some());
    assert!(s.remote().trash(&VPath::parse("/project").unwrap()).is_err());
    assert!(s.remote().rename(&VPath::parse("/project").unwrap(), &VPath::parse("/p2").unwrap()).is_err());
    assert!(s.remote().trash_list().is_err());
    // Adding data is allowed.
    let r = send::send(s.remote(), &[src], &VPath::parse("/copy").unwrap(), &opts(Mode::Copy), &mut |_| {}).unwrap();
    assert!(r.ok());
}

#[test]
fn a_pack_in_use_is_never_sealed_by_someone_else() {
    let (_tmp, src, root) = setup();
    let mut a = Store::init(&root, Config::default()).unwrap();
    let (_, project) = catalog::create_project(a.conn(), catalog::ROOT_ID, "p", 0, 0o755).unwrap();
    let req = PutFile {
        dest: VPath::parse("/p/a.fastq").unwrap(),
        meta: FileMeta { size: fs::metadata(src.join("sub/big_text.fastq")).unwrap().len(), mtime_ns: 0, mode: 0o644 },
        policy: Conflict::Skip,
    };
    let mut p = CompressingReader::new(fs::File::open(src.join("sub/big_text.fastq")).unwrap(), CompressOptions::default()).unwrap();
    a.put_file("job", &req, &mut p).unwrap();
    let held = catalog::open_pack_for(a.conn(), "job", &project).unwrap().unwrap();

    // Another process (the worker) tries to seal every open pack: it must skip this one.
    let mut b = Store::open(&root).unwrap();
    assert_eq!(b.seal_all_open(None).unwrap(), 0);
    assert_eq!(catalog::pack(b.conn(), held.id).unwrap().state, PackState::Open);

    // A second session for the same job writes to a new pack instead.
    let req2 =
        PutFile { dest: VPath::parse("/p/b.csv").unwrap(), meta: FileMeta { size: 3, mtime_ns: 0, mode: 0o644 }, policy: Conflict::Skip };
    let mut p2 = CompressingReader::new(&b"abc"[..], CompressOptions::default()).unwrap();
    b.put_file("job", &req2, &mut p2).unwrap();
    let packs = catalog::packs(b.conn()).unwrap();
    assert_eq!(packs.iter().filter(|p| p.state == PackState::Open).count(), 2);

    // Once the first session is gone, finishing the job seals both packs.
    drop(a);
    b.finish_job("job").unwrap();
    assert_eq!(b.seal_all_open(None).unwrap(), 0);
    assert!(catalog::packs(b.conn()).unwrap().iter().all(|p| p.state == PackState::Sealed));
}

#[test]
fn worker_protects_snapshots_and_reports() {
    if archive_core::par2::Par2::find(None).is_err() {
        eprintln!("skipping: par2 not installed");
        return;
    }
    let (_tmp, src, root) = setup();
    let mut store = Store::init(&root, Config { pack_target_bytes: 1 << 20, ..Config::default() }).unwrap();
    send::send(&mut store, &[src], &VPath::root(), &opts(Mode::Copy), &mut |_| {}).unwrap();

    // While this session holds the lock file, another run does nothing.
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(root.join("worker.lock")).unwrap();
    assert!(archive_core::util::try_lock(&lock).unwrap());
    assert!(worker::run(&root, &WorkerOptions::default()).unwrap().is_none());
    drop(lock);

    let st = worker::run(&root, &WorkerOptions { force_daily: true, ..WorkerOptions::default() }).unwrap().unwrap();
    assert!(st.errors.is_empty(), "{:?}", st.errors);
    assert!(st.protected.iter().all(|r| r.outcome == "protected"));
    assert_eq!(st.packs_protected, st.packs_total);
    assert_eq!(st.packs_pending, 0);
    let snap = PathBuf::from(st.snapshot.unwrap());
    assert!(snap.exists());
    assert!(snap.with_extension("db.par2").exists());
    assert!(worker::read_status(&root).is_some());
    // The snapshot is a usable catalog.
    let copy = archive_core::catalog::Catalog::open(&snap).unwrap();
    assert!(catalog::entry(copy.conn(), catalog::ROOT_ID).unwrap().files > 0);

    // Protected packs just checked aren't re-scrubbed on the next run.
    let st2 = worker::run(&root, &WorkerOptions::default()).unwrap().unwrap();
    assert!(st2.scrubbed.is_empty(), "{:?}", st2.scrubbed);
}

#[test]
fn batched_reads_report_per_file_errors_and_stay_in_step() {
    let (_tmp, src, root) = setup();
    let mut store = Store::init(&root, Config::default()).unwrap();
    send::send(&mut store, &[src.clone()], &VPath::root(), &opts(Mode::Copy), &mut |_| {}).unwrap();
    drop(store);
    let mut s = start(&root, false);
    let paths =
        ["/project/small.csv", "/project/missing.txt", "/project/sub/random.bin", "/project/empty.txt"].map(|p| VPath::parse(p).unwrap());
    let mut got: Vec<Option<Vec<u8>>> = vec![None; 4];
    s.remote()
        .read_files(&paths, &mut |i, res| {
            if let Ok((_, r)) = res {
                let mut v = Vec::new();
                r.read_to_end(&mut v).unwrap();
                got[i] = Some(v);
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(got[0].as_deref(), Some(&fs::read(src.join("small.csv")).unwrap()[..]));
    assert!(got[1].is_none());
    assert_eq!(got[2].as_deref(), Some(&fs::read(src.join("sub/random.bin")).unwrap()[..]));
    assert_eq!(got[3].as_deref(), Some(&b""[..]));
    assert!(s.remote().stat(&VPath::parse("/project").unwrap()).unwrap().is_some());
}
