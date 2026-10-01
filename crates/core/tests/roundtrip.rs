//! End-to-end tests of the local engine: send, retrieve, Move safety, resume,
//! dedup, conflicts, corruption in transit and at rest, crash recovery, and
//! recovery with only standard tools.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::*;

use archive_core::api::{Archive, DirSpec, Payload, PutFile, PutOutcome, PutSolid};
use archive_core::catalog::{self, Entry, NodeKind, PackState};
use archive_core::hash::Digest;
use archive_core::retrieve::{self, LocalConflict, RetrieveOptions};
use archive_core::seekable::CompressOptions;
use archive_core::send::{self, Event, Mode, SendOptions};
use archive_core::store::{Config, Store};
use archive_core::{Conflict, Error, PutStatus, Result, VPath};

fn small_config() -> Config {
    Config { pack_target_bytes: 1 << 20, ..Config::default() }
}

fn opts(mode: Mode) -> SendOptions {
    SendOptions {
        mode,
        compress: CompressOptions { level: 3, frame_size: 64 << 10, threads: 3 },
        solid_block_max: 256 << 10,
        ..SendOptions::default()
    }
}

fn quiet() -> impl FnMut(&Event) {
    |_| {}
}

fn send_dir(store: &mut dyn Archive, src: &Path, dest: &str, o: &SendOptions) -> send::SendReport {
    send::send(store, &[src.to_path_buf()], &VPath::parse(dest).unwrap(), o, &mut quiet()).unwrap()
}

fn get(store: &mut dyn Archive, src: &str, dest: &Path) -> retrieve::RetrieveReport {
    retrieve::retrieve(store, &VPath::parse(src).unwrap(), dest, &RetrieveOptions::default(), &mut |_| {}).unwrap()
}

struct Env {
    _tmp: tempfile::TempDir,
    src: PathBuf,
    arc: PathBuf,
    out: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("project");
    make_tree(&src);
    Env { src, arc: tmp.path().join("archive"), out: tmp.path().join("out"), _tmp: tmp }
}

#[test]
fn copy_round_trip_preserves_everything() {
    let e = env();
    let before = snapshot(&e.src);
    let mut store = Store::init(&e.arc, small_config()).unwrap();
    let r = send_dir(&mut store, &e.src, "/Projects", &opts(Mode::Copy));
    assert!(r.ok(), "{:?}", r.failed);
    assert_eq!(r.deduplicated + r.stored + r.identical, r.files + 1 /* the symlink */, "{r:?}");
    assert_eq!(r.deleted, 0);
    assert!(e.src.join("small.csv").exists(), "copy must not delete");
    assert!(r.bytes_sent < r.bytes, "compression should help: {} vs {}", r.bytes_sent, r.bytes);

    // Multiple packs, all sealed.
    let packs = catalog::packs(store.conn()).unwrap();
    assert!(packs.len() > 1, "expected pack rotation");
    assert!(packs.iter().all(|p| p.state == PackState::Sealed));

    let g = get(&mut store, "/Projects/project", &e.out);
    assert!(g.ok(), "{:?}", g.failed);
    assert_same(&snapshot(&e.out.join("project")), &before);

    // Single file and subfolder retrieval.
    let one = e.out.join("one");
    get(&mut store, "/Projects/project/sub/random.bin", &one);
    assert_eq!(fs::read(one.join("random.bin")).unwrap(), fs::read(e.src.join("sub/random.bin")).unwrap());
    get(&mut store, "/Projects/project/sub/deeper", &one);
    assert_eq!(fs::read(one.join("deeper/notes.md")).unwrap(), fs::read(e.src.join("sub/deeper/notes.md")).unwrap());

    // Folder totals match what was sent.
    let top = store.stat(&VPath::parse("/Projects/project").unwrap()).unwrap().unwrap();
    assert_eq!(top.size, r.bytes);
    assert_eq!(top.files, r.files);
}

#[test]
fn move_deletes_sources_and_emptied_folders() {
    let e = env();
    let before = snapshot(&e.src);
    let mut store = Store::init(&e.arc, small_config()).unwrap();
    let r = send_dir(&mut store, &e.src, "/", &opts(Mode::Move));
    assert!(r.ok(), "{:?}", r.failed);
    assert!(r.kept.is_empty(), "{:?}", r.kept);
    // Everything moved; folders that only held OS junk are gone too.
    assert!(!e.src.exists(), "source tree should be gone: {:?}", fs::read_dir(&e.src).map(|d| d.count()));
    get(&mut store, "/project", &e.out);
    assert_same(&snapshot(&e.out.join("project")), &before);
}

#[test]
fn resend_is_identical_and_dedup_links() {
    let e = env();
    let mut store = Store::init(&e.arc, small_config()).unwrap();
    let first = send_dir(&mut store, &e.src, "/A", &opts(Mode::Copy));
    assert_eq!(first.deduplicated, 1, "dup_b.dat duplicates dup_a.dat within one transfer: {first:?}");
    let bytes_after_first: u64 = catalog::packs(store.conn()).unwrap().iter().map(|p| p.bytes).sum();

    // Same destination again: nothing is re-sent.
    let again = send_dir(&mut store, &e.src, "/A", &opts(Mode::Copy));
    assert_eq!(again.identical, again.files + 1, "{again:?}");
    assert_eq!(again.bytes_sent, 0);

    // Sent again elsewhere, it's a second project, which stores its own copy:
    // projects never share data, so each can be recovered or deleted alone.
    let copy = send_dir(&mut store, &e.src, "/B", &opts(Mode::Copy));
    assert_eq!((copy.stored, copy.deduplicated), (first.stored, 1), "{copy:?}");
    assert!(copy.bytes_sent > first.bytes_sent * 9 / 10);
    let bytes_after: u64 = catalog::packs(store.conn()).unwrap().iter().map(|p| p.bytes).sum();
    assert!(bytes_after >= 2 * bytes_after_first - 200_000);
    let a = store.stat(&VPath::parse("/A/project").unwrap()).unwrap().unwrap();
    let b = store.stat(&VPath::parse("/B/project").unwrap()).unwrap().unwrap();
    assert!(a.is_project && b.is_project && a.project != b.project);
    assert!(catalog::packs(store.conn()).unwrap().iter().all(|p| p.project == a.project || p.project == b.project));

    get(&mut store, "/B/project", &e.out);
    assert_same(&snapshot(&e.out.join("project")), &snapshot(&e.src));
}

#[test]
fn conflict_policies() {
    let e = env();
    let mut store = Store::init(&e.arc, small_config()).unwrap();
    send_dir(&mut store, &e.src, "/", &opts(Mode::Copy));
    fs::write(e.src.join("small.csv"), b"changed contents").unwrap();

    let mut o = opts(Mode::Move);
    o.policy = Conflict::Skip;
    let r = send_dir(&mut store, &e.src, "/", &o);
    assert_eq!(r.skipped.len(), 1);
    assert!(e.src.join("small.csv").exists(), "skipped file must not be deleted in Move mode");

    let mut o = opts(Mode::Copy);
    o.policy = Conflict::KeepBoth;
    fs::create_dir_all(e.src.join("x")).ok();
    let r = send::send(&mut store, &[e.src.join("small.csv")], &VPath::parse("/project").unwrap(), &o, &mut quiet()).unwrap();
    assert_eq!(r.stored, 1);
    assert!(store.stat(&VPath::parse("/project/small (2).csv").unwrap()).unwrap().is_some());

    // Archived projects are frozen: nothing in them is ever replaced.
    let before = store.read_file(&VPath::parse("/project/small.csv").unwrap()).unwrap().0.sha256;
    o.policy = Conflict::Replace;
    fs::write(e.src.join("small.csv"), b"third version").unwrap();
    let r = send::send(&mut store, &[e.src.join("small.csv")], &VPath::parse("/project").unwrap(), &o, &mut quiet()).unwrap();
    assert_eq!(r.failed.len(), 1, "{r:?}");
    assert!(r.failed[0].1.contains("can't be changed"), "{r:?}");
    assert_eq!(store.read_file(&VPath::parse("/project/small.csv").unwrap()).unwrap().0.sha256, before);
    assert!(catalog::children(store.conn(), catalog::TRASH_ID).unwrap().is_empty());
}

/// Wraps a store to inject faults.
struct Faulty<'a> {
    inner: &'a mut Store,
    /// Flip a byte in the payload of this many puts.
    corrupt_puts: u32,
    /// Modify this local file right after it is archived.
    touch_after_put: Option<PathBuf>,
}

struct Flip<'a> {
    inner: &'a mut dyn Payload,
    pos: u64,
    at: u64,
}

impl Read for Flip<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if self.at >= self.pos && self.at < self.pos + n as u64 {
            buf[(self.at - self.pos) as usize] ^= 0x55;
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl Payload for Flip<'_> {
    fn digest(&self) -> Option<Digest> {
        self.inner.digest()
    }
}

impl Archive for Faulty<'_> {
    fn stat(&mut self, p: &VPath) -> Result<Option<Entry>> {
        self.inner.stat(p)
    }
    fn list(&mut self, p: &VPath) -> Result<Vec<Entry>> {
        self.inner.list(p)
    }
    fn walk(&mut self, p: &VPath) -> Result<Vec<(String, Entry)>> {
        self.inner.walk(p)
    }
    fn sizes_present(&mut self, scope: &VPath, s: &[u64]) -> Result<Vec<bool>> {
        self.inner.sizes_present(scope, s)
    }
    fn have(&mut self, scope: &VPath, h: &[Digest]) -> Result<Vec<bool>> {
        self.inner.have(scope, h)
    }
    fn mkdirs(&mut self, job: &str, d: &[DirSpec]) -> Result<()> {
        self.inner.mkdirs(job, d)
    }
    fn symlink(&mut self, job: &str, d: &VPath, t: &str, m: i64, p: Conflict) -> Result<PutOutcome> {
        self.inner.symlink(job, d, t, m, p)
    }
    fn link(&mut self, job: &str, r: &PutFile, s: &Digest) -> Result<PutOutcome> {
        self.inner.link(job, r, s)
    }
    fn put_file(&mut self, job: &str, r: &PutFile, payload: &mut dyn Payload) -> Result<PutOutcome> {
        let out = if self.corrupt_puts > 0 {
            self.corrupt_puts -= 1;
            self.inner.put_file(job, r, &mut Flip { inner: payload, pos: 0, at: 100 })
        } else {
            self.inner.put_file(job, r, payload)
        };
        if let Some(p) = &self.touch_after_put {
            let mut f = fs::OpenOptions::new().append(true).open(p).unwrap();
            f.write_all(b"!").unwrap();
        }
        out
    }
    fn put_solid(&mut self, job: &str, r: &PutSolid, payload: &mut dyn Payload) -> Result<Vec<PutOutcome>> {
        if self.corrupt_puts > 0 {
            self.corrupt_puts -= 1;
            return self.inner.put_solid(job, r, &mut Flip { inner: payload, pos: 0, at: 40 });
        }
        self.inner.put_solid(job, r, payload)
    }
    fn settle(&mut self) -> Result<Vec<(VPath, String)>> {
        self.inner.settle()
    }
    fn finish_job(&mut self, job: &str) -> Result<()> {
        self.inner.finish_job(job)
    }
    fn read_file(&mut self, p: &VPath) -> Result<(Entry, Box<dyn Read + Send + '_>)> {
        self.inner.read_file(p)
    }
    fn info(&mut self, p: &VPath) -> Result<archive_core::maint::Info> {
        self.inner.info(p)
    }
    fn search(&mut self, q: &str, l: usize) -> Result<Vec<(VPath, Entry)>> {
        Archive::search(self.inner, q, l)
    }
    fn create_folder(&mut self, p: &VPath) -> Result<()> {
        self.inner.create_folder(p)
    }
    fn rename(&mut self, a: &VPath, b: &VPath) -> Result<()> {
        Archive::rename(self.inner, a, b)
    }
    fn trash(&mut self, p: &VPath) -> Result<i64> {
        Archive::trash(self.inner, p)
    }
    fn restore(&mut self, id: i64, to: Option<&VPath>) -> Result<VPath> {
        Archive::restore(self.inner, id, to)
    }
    fn trash_list(&mut self) -> Result<Vec<Entry>> {
        Archive::trash_list(self.inner)
    }
}

#[test]
fn corruption_in_transit_is_rejected_and_nothing_is_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("big");
    write(&src.join("a.bin"), &text(2 << 20, 9));
    let mut store = Store::init(&tmp.path().join("arc"), small_config()).unwrap();

    // Every attempt corrupted: the file fails, is not committed, and is not deleted.
    let mut f = Faulty { inner: &mut store, corrupt_puts: 10, touch_after_put: None };
    let r = send::send(&mut f, &[src.clone()], &VPath::root(), &opts(Mode::Move), &mut quiet()).unwrap();
    assert_eq!(r.failed.len(), 1, "{r:?}");
    assert!(src.join("a.bin").exists());
    assert!(store.stat(&VPath::parse("/big/a.bin").unwrap()).unwrap().is_none());

    // One corrupted attempt, then a clean retry: archived and moved.
    let mut f = Faulty { inner: &mut store, corrupt_puts: 1, touch_after_put: None };
    let r = send::send(&mut f, &[src.clone()], &VPath::root(), &opts(Mode::Move), &mut quiet()).unwrap();
    assert!(r.ok(), "{r:?}");
    assert!(!src.join("a.bin").exists());

    // Every pack is still a valid tar after the rollbacks.
    for p in catalog::packs(store.conn()).unwrap() {
        let mut fh = fs::File::open(store.pack_path(&p.name)).unwrap();
        archive_core::tar::list(&mut fh, 0, None).unwrap();
    }
}

#[test]
fn file_modified_after_read_is_not_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("d");
    let victim = src.join("growing.log");
    write(&victim, &text(2 << 20, 11));
    let mut store = Store::init(&tmp.path().join("arc"), small_config()).unwrap();
    let mut f = Faulty { inner: &mut store, corrupt_puts: 0, touch_after_put: Some(victim.clone()) };
    let r = send::send(&mut f, &[src.clone()], &VPath::root(), &opts(Mode::Move), &mut quiet()).unwrap();
    assert!(victim.exists(), "a file that changed must never be deleted");
    assert!(!r.failed.is_empty() || !r.kept.is_empty(), "{r:?}");
}

#[test]
fn interrupted_write_is_truncated_on_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("d");
    write(&src.join("one.bin"), &text(1 << 20, 12));
    write(&src.join("two.bin"), &text(1 << 20, 14));
    let arc = tmp.path().join("arc");
    let mut store = Store::init(&arc, Config { pack_target_bytes: 1 << 40, ..Config::default() }).unwrap();
    let mut o = opts(Mode::Copy);
    o.job = "job-1".into();
    o.small_file_max = 0;

    // Archive one file but don't seal (simulate a crash before finish_job).
    let (_, project) = catalog::create_project(store.conn(), catalog::ROOT_ID, "d", 0, 0o755).unwrap();
    let req = PutFile {
        dest: VPath::parse("/d/one.bin").unwrap(),
        meta: archive_core::FileMeta { size: 1 << 20, mtime_ns: 0, mode: 0o644 },
        policy: Conflict::Skip,
    };
    let mut payload = archive_core::seekable::CompressingReader::new(fs::File::open(src.join("one.bin")).unwrap(), o.compress).unwrap();
    store.put_file("job-1", &req, &mut payload).unwrap();
    let pack = catalog::open_pack_for(store.conn(), "job-1", &project).unwrap().unwrap();
    drop(store);

    // Garbage after the committed length, as if a later write was cut off.
    let path = arc.join("packs").join(&pack.name);
    fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(&[0xAB; 3000]).unwrap();

    let mut store = Store::open(&arc).unwrap();
    let r = send_dir(&mut store, &src, "/", &o);
    assert!(r.ok(), "{r:?}");
    assert_eq!(r.identical, 1, "{r:?}");
    assert_eq!(r.stored, 1);
    let sealed = catalog::pack(store.conn(), pack.id).unwrap();
    assert_eq!(sealed.state, PackState::Sealed);
    assert_eq!(fs::metadata(&path).unwrap().len(), sealed.bytes);
    let names: Vec<String> =
        archive_core::tar::list(&mut fs::File::open(&path).unwrap(), 0, None).unwrap().into_iter().map(|e| e.path).collect();
    assert!(names.contains(&"d/one.bin.zst".to_string()) && names.contains(&"d/two.bin.zst".to_string()), "{names:?}");
}

#[test]
fn damage_at_rest_is_detected_on_retrieve() {
    let e = env();
    let mut store = Store::init(&e.arc, Config::default()).unwrap();
    send_dir(&mut store, &e.src, "/", &opts(Mode::Copy));
    let entry = store.stat(&VPath::parse("/project/sub/big_text.fastq").unwrap()).unwrap().unwrap();
    let loc = catalog::location(store.conn(), entry.content_id.unwrap()).unwrap();
    let path = store.pack_path(&loc.pack_name);
    let mut bytes = fs::read(&path).unwrap();
    bytes[(loc.data_off + 5000) as usize] ^= 0xFF;
    fs::write(&path, bytes).unwrap();

    let r = retrieve::retrieve(
        &mut store,
        &VPath::parse("/project/sub/big_text.fastq").unwrap(),
        &e.out,
        &RetrieveOptions { policy: LocalConflict::Skip, retries: 0, cancel: None },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(r.failed.len(), 1);
    assert!(!e.out.join("big_text.fastq").exists(), "a damaged file must not appear under its real name");
    let leftovers: Vec<_> = fs::read_dir(&e.out).unwrap().collect();
    assert!(leftovers.is_empty(), "no partial files left behind");
}

#[test]
fn standard_tools_can_recover_every_pack() {
    let have = |c: &str| Command::new(c).arg("--version").output().is_ok();
    if !have("zstd") || !have("tar") {
        eprintln!("skipping: zstd or tar not installed");
        return;
    }
    let e = env();
    let before = snapshot(&e.src);
    let mut store = Store::init(&e.arc, small_config()).unwrap();
    let r = send_dir(&mut store, &e.src, "/", &opts(Mode::Copy));
    assert!(r.ok());
    assert!(r.deduplicated > 0, "the tree has duplicates, which recover.sh must recreate");

    let recovered = e.out.join("recovered");
    fs::create_dir_all(&recovered).unwrap();
    let script = e.out.join("recover.sh");
    fs::write(&script, archive_core::recovery::RECOVER_SH).unwrap();
    let mut packs = catalog::packs(store.conn()).unwrap();
    packs.sort_by_key(|p| p.id);
    for p in packs {
        let st = Command::new("sh").arg(&script).arg(store.pack_path(&p.name)).arg(&recovered).output().unwrap();
        assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
    }
    fs::remove_dir_all(recovered.join(".archive")).unwrap();
    let got = snapshot(&recovered.join("project"));
    // Compare content and links (tar keeps whole-second mtimes, which snapshot uses too).
    assert_same(&got, &before);
}

#[test]
fn trash_and_restore_through_the_store() {
    let e = env();
    let mut store = Store::init(&e.arc, small_config()).unwrap();
    send_dir(&mut store, &e.src, "/", &opts(Mode::Copy));
    let sub = store.stat(&VPath::parse("/project/sub").unwrap()).unwrap().unwrap();
    let total = store.stat(&VPath::root()).unwrap().unwrap().files;
    catalog::trash(store.conn(), sub.id).unwrap();
    assert!(store.stat(&VPath::parse("/project/sub").unwrap()).unwrap().is_none());
    assert!(store.stat(&VPath::root()).unwrap().unwrap().files < total);
    catalog::restore(store.conn(), sub.id, None).unwrap();
    assert_eq!(store.stat(&VPath::root()).unwrap().unwrap().files, total);
    let g = get(&mut store, "/project/sub", &e.out);
    assert!(g.ok());
    let _ = NodeKind::Dir;
    let _ = PutStatus::Stored;
    let _: Option<Error> = None;
}

#[test]
fn cancel_stops_cleanly_and_keeps_unsent_sources() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("many");
    for i in 0..6 {
        write(&src.join(format!("f{i}.bin")), &text(2 << 20, 101 + 2 * i));
    }
    let mut store = Store::init(&tmp.path().join("arc"), small_config()).unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    let mut o = opts(Mode::Move);
    o.small_file_max = 0;
    o.cancel = Some(flag.clone());
    let mut archived = 0;
    let r = send::send(&mut store, &[src.clone()], &VPath::root(), &o, &mut |e| {
        if let Event::Archived { .. } = e {
            archived += 1;
            if archived == 2 {
                flag.store(true, Ordering::Relaxed);
            }
        }
    })
    .unwrap();
    assert!(r.cancelled);
    assert_eq!(r.stored, 2);
    // A stopped Move deletes nothing: the originals stay until the whole transfer is done.
    assert_eq!(r.deleted, 0);
    assert!(r.originals_kept);
    assert_eq!(fs::read_dir(&src).unwrap().count(), 6, "every original stays");
    // Everything that was archived is sealed and intact.
    assert!(catalog::packs(store.conn()).unwrap().iter().all(|p| p.state == PackState::Sealed));
    // Resuming sends the other four, recognizes the two already archived, then deletes all six.
    let r2 = send::send(&mut store, &[src.clone()], &VPath::root(), &opts(Mode::Move), &mut |_| {}).unwrap();
    assert!(r2.ok() && r2.stored == 4 && r2.identical == 2 && r2.deleted == 6, "{r2:?}");
    assert!(!src.exists(), "the emptied folder is removed");
}

#[test]
fn project_folders_are_reported_new_only_once() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("run7");
    write(&src.join("a.txt"), b"alpha");
    let mut store = Store::init(&tmp.path().join("arc"), small_config()).unwrap();
    let projects = |store: &mut Store| {
        let mut seen = Vec::new();
        send::send(store, &[src.clone()], &VPath::root(), &opts(Mode::Copy), &mut |e| {
            if let Event::Project { path, new } = e {
                seen.push((path.to_string(), *new));
            }
        })
        .unwrap();
        seen
    };
    assert_eq!(projects(&mut store), vec![("/run7".to_string(), true)]);
    // Sending again (or resuming) finds it already there, so it isn't this attempt's to remove.
    assert_eq!(projects(&mut store), vec![("/run7".to_string(), false)]);
}

#[test]
fn a_failed_check_resends_it_and_everything_after_it() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("many");
    for i in 0..6 {
        write(&src.join(format!("f{i}.bin")), &text(2 << 20, 31 + i));
    }
    let mut store = Store::init(&tmp.path().join("arc"), small_config()).unwrap();
    let mut o = opts(Mode::Move);
    o.small_file_max = 0;
    // The first file is corrupted on the way. Files sent after it, while its
    // check ran, are discarded with it, then everything is sent again.
    let mut f = Faulty { inner: &mut store, corrupt_puts: 1, touch_after_put: None };
    let r = send::send(&mut f, &[src.clone()], &VPath::root(), &o, &mut quiet()).unwrap();
    assert!(r.ok(), "{r:?}");
    assert_eq!(r.deleted, 6);
    for i in 0..6 {
        let e = store.stat(&VPath::parse(&format!("/many/f{i}.bin")).unwrap()).unwrap();
        assert!(e.is_some_and(|e| e.size == 2 << 20), "f{i}");
    }
    for p in catalog::packs(store.conn()).unwrap() {
        let mut fh = fs::File::open(store.pack_path(&p.name)).unwrap();
        archive_core::tar::list(&mut fh, 0, None).unwrap();
    }
}
