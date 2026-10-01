//! Plain copies and moves (`copy`): this computer and file servers in every
//! combination, conflicts, moving, resuming, cancelling, and corruption.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

mod common;
use common::*;

use archive_core::copy::{self, CopyOptions, CopyReport, Endpoint, Event, FileStat, LocalEnd, Walk, part_string};
use archive_core::fileserve::{self, FilesOptions};
use archive_core::hash::Digest;
use archive_core::remote::Remote;
use archive_core::retrieve::LocalConflict;
use archive_core::send::Mode;
use archive_core::{Error, Result};

/// A file server running on a thread, and a client connected to it.
struct Files {
    client: Option<Remote>,
    thread: Option<JoinHandle<archive_core::Result<()>>>,
}

impl Files {
    fn start(home: &Path) -> Files {
        let (c2s_r, c2s_w) = io::pipe().unwrap();
        let (s2c_r, s2c_w) = io::pipe().unwrap();
        let opts =
            FilesOptions { start: home.to_path_buf(), helper_version: "test".into(), start_job: Box::new(|_| {}), askpass: PathBuf::new() };
        let thread = std::thread::spawn(move || fileserve::serve_files(c2s_r, s2c_w, opts));
        let client = Remote::connect(Box::new(s2c_r), Box::new(c2s_w)).unwrap();
        Files { client: Some(client), thread: Some(thread) }
    }
    fn end(&mut self) -> &mut Remote {
        self.client.as_mut().unwrap()
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        drop(self.client.take());
        if let Some(t) = self.thread.take() {
            t.join().unwrap().unwrap();
        }
    }
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

/// The source tree without its symbolic link (which a copy leaves behind).
fn tree_snapshot(root: &Path) -> std::collections::BTreeMap<String, Node> {
    let mut snap = snapshot(root);
    snap.retain(|_, n| !matches!(n, Node::Link(_)));
    snap
}

fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("project");
    make_tree(&src);
    let dest = tmp.path().join("dest");
    fs::create_dir_all(&dest).unwrap();
    (tmp, src, dest)
}

fn run(
    src: &mut dyn Endpoint,
    dst: &mut dyn Endpoint,
    sources: &[String],
    dest: &Path,
    opts: &CopyOptions,
) -> (Result<CopyReport>, Vec<Event>) {
    let mut events = Vec::new();
    let r = copy::copy(src, dst, sources, &s(dest), opts, &mut |e| events.push(e.clone()));
    (r, events)
}

#[test]
fn local_to_local_copy_is_exact() {
    let (_tmp, src, dest) = setup();
    let (r, events) = run(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&src)], &dest, &CopyOptions::default());
    let report = r.unwrap();
    assert!(report.ok(), "{report:?}");
    assert_same(&snapshot(&dest.join("project")), &tree_snapshot(&src));
    assert!(report.skipped.iter().any(|(_, why)| why.contains("shortcut")), "the link is reported, not followed");
    assert!(events.iter().any(|e| matches!(e, Event::Scanned { .. })));
    // The junk files (.DS_Store, ._x) are not copied.
    assert!(!dest.join("project/.DS_Store").exists());
    assert!(!dest.join("project/sub/._random.bin").exists());
    // Nothing left over.
    assert!(fs::read_dir(dest.join("project")).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".qm-part")));
}

#[test]
fn through_a_file_server_and_back() {
    let (tmp, src, _dest) = setup();
    let home = tmp.path().join("server_home");
    fs::create_dir_all(&home).unwrap();
    let mut server = Files::start(&home);

    // Up.
    let (r, _) = run(&mut LocalEnd::new(), server.end(), &[s(&src)], &home, &CopyOptions::default());
    assert!(r.unwrap().ok());
    assert_same(&snapshot(&home.join("project")), &tree_snapshot(&src));

    // And down again, to a new folder.
    let back = tmp.path().join("back");
    fs::create_dir_all(&back).unwrap();
    let (r, _) = run(server.end(), &mut LocalEnd::new(), &[s(&home.join("project"))], &back, &CopyOptions::default());
    assert!(r.unwrap().ok());
    assert_same(&snapshot(&back.join("project")), &tree_snapshot(&src));
}

#[test]
fn server_to_server_goes_through_this_computer() {
    let (tmp, src, _dest) = setup();
    let (home_a, home_b) = (tmp.path().join("a"), tmp.path().join("b"));
    fs::create_dir_all(&home_a).unwrap();
    fs::create_dir_all(&home_b).unwrap();
    let mut a = Files::start(&home_a);
    let mut b = Files::start(&home_b);
    let (r, _) = run(&mut LocalEnd::new(), a.end(), &[s(&src)], &home_a, &CopyOptions::default());
    assert!(r.unwrap().ok());
    let (r, _) = run(a.end(), b.end(), &[s(&home_a.join("project"))], &home_b, &CopyOptions::default());
    let report = r.unwrap();
    assert!(report.ok(), "{report:?}");
    assert_same(&snapshot(&home_b.join("project")), &tree_snapshot(&src));
}

#[test]
fn several_sources_and_a_single_file() {
    let (_tmp, src, dest) = setup();
    let sources = [s(&src.join("small.csv")), s(&src.join("sub")), s(&src.join("empty_dir"))];
    let (r, _) = run(&mut LocalEnd::new(), &mut LocalEnd::new(), &sources, &dest, &CopyOptions::default());
    assert!(r.unwrap().ok());
    assert_eq!(fs::read(dest.join("small.csv")).unwrap(), fs::read(src.join("small.csv")).unwrap());
    assert!(dest.join("sub/deeper/notes.md").exists());
    assert!(dest.join("empty_dir").is_dir());
}

#[test]
fn conflicts_follow_the_policy() {
    let (_tmp, src, dest) = setup();
    let one = [s(&src.join("small.csv"))];
    let opts = |policy| CopyOptions { policy, ..CopyOptions::default() };
    let mut l = (LocalEnd::new(), LocalEnd::new());

    // A different file is already there.
    fs::write(dest.join("small.csv"), b"mine").unwrap();
    let (r, _) = run(&mut l.0, &mut l.1, &one, &dest, &opts(LocalConflict::Skip));
    let report = r.unwrap();
    assert_eq!(report.copied, 0);
    assert_eq!(fs::read(dest.join("small.csv")).unwrap(), b"mine");

    let (r, _) = run(&mut l.0, &mut l.1, &one, &dest, &opts(LocalConflict::KeepBoth));
    assert_eq!(r.unwrap().copied, 1);
    assert_eq!(fs::read(dest.join("small.csv")).unwrap(), b"mine");
    assert_eq!(fs::read(dest.join("small (2).csv")).unwrap(), fs::read(src.join("small.csv")).unwrap());

    let (r, events) = run(&mut l.0, &mut l.1, &one, &dest, &opts(LocalConflict::Replace));
    assert_eq!(r.unwrap().copied, 1);
    assert_eq!(fs::read(dest.join("small.csv")).unwrap(), fs::read(src.join("small.csv")).unwrap());
    // A replaced file existed before, so Abandon must not delete it.
    assert!(!events.iter().any(|e| matches!(e, Event::Created { is_dir: false, .. })));

    // The same file (size and date) is left alone by Skip and Replace, so a repeated or
    // resumed copy doesn't redo work.
    for policy in [LocalConflict::Skip, LocalConflict::Replace] {
        let (r, _) = run(&mut l.0, &mut l.1, &one, &dest, &opts(policy));
        let report = r.unwrap();
        assert_eq!(report.copied, 0, "{policy:?}");
        assert!(report.skipped.iter().any(|(_, why)| why == "already there"));
    }
    assert!(!dest.join("small (3).csv").exists());
    // Keep both is a request for a second copy, identical or not.
    let (r, _) = run(&mut l.0, &mut l.1, &one, &dest, &opts(LocalConflict::KeepBoth));
    assert_eq!(r.unwrap().copied, 1);
    assert!(dest.join("small (3).csv").exists());
}

#[test]
fn move_deletes_nothing_until_every_file_has_arrived() {
    let (_tmp, src, dest) = setup();
    let folder = src.join("sub");
    let opts = CopyOptions { mode: Mode::Move, ..CopyOptions::default() };
    let mut copied = 0;
    let mut first_removal_after = None;
    let r = copy::copy(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&folder)], &s(&dest), &opts, &mut |e| match e {
        Event::Copied { .. } => {
            copied += 1;
            // Every original is still in place while files are being copied.
            assert!(folder.join("random.bin").exists() && folder.join("big_text.fastq").exists());
        }
        Event::Removed { .. } => {
            first_removal_after.get_or_insert(copied);
        }
        _ => {}
    });
    let report = r.unwrap();
    assert!(report.ok(), "{report:?}");
    assert_eq!(first_removal_after, Some(copied), "the first deletion comes after the last copy");
    assert_eq!(report.removed, 3);
    assert!(!folder.exists(), "and the emptied folders go with them");
    assert!(dest.join("sub/random.bin").exists());
}

#[test]
fn a_failure_means_nothing_is_deleted() {
    let (_tmp, src, dest) = setup();
    let folder = src.join("sub");
    fs::write(folder.join("changing.txt"), b"first").unwrap();
    let changing_path = s(&folder.join("changing.txt"));
    let opts = CopyOptions { mode: Mode::Move, ..CopyOptions::default() };
    let r = copy::copy(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&folder)], &s(&dest), &opts, &mut |e| {
        // Change the file after it is scanned but before it is copied.
        if matches!(e, Event::Scanned { .. }) {
            fs::write(&changing_path, b"second, longer").unwrap();
        }
    });
    let report = r.unwrap();
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(report.originals_kept);
    assert_eq!(report.removed, 0);
    // The others were copied, but every original is still there.
    assert!(dest.join("sub/random.bin").exists());
    assert!(folder.join("random.bin").exists());
    assert!(folder.join("deeper/notes.md").exists());
    assert_eq!(fs::read(folder.join("changing.txt")).unwrap(), b"second, longer");
    assert!(!dest.join("sub/changing.txt").exists());
}

#[test]
fn stopping_a_move_deletes_nothing() {
    let (_tmp, src, dest) = setup();
    let flag = Arc::new(AtomicBool::new(false));
    let opts = CopyOptions { mode: Mode::Move, cancel: Some(flag.clone()), ..CopyOptions::default() };
    let mut copied = 0;
    let r = copy::copy(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&src)], &s(&dest), &opts, &mut |e| {
        if matches!(e, Event::Copied { .. }) {
            copied += 1;
            if copied == 3 {
                flag.store(true, Ordering::Relaxed);
            }
        }
    });
    let report = r.unwrap();
    assert!(report.cancelled);
    assert_eq!(report.removed, 0);
    assert!(report.originals_kept);
    assert!(src.join("sub/random.bin").exists() && src.join("small.csv").exists());
}

#[test]
fn a_move_that_was_interrupted_after_copying_carries_on() {
    let (_tmp, src, dest) = setup();
    let folder = src.join("sub");
    // First run: a plain copy stands in for a Move that was cut off before deleting anything.
    let (r, _) = run(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&folder)], &dest, &CopyOptions::default());
    assert!(r.unwrap().ok());
    assert!(folder.join("random.bin").exists());

    let opts = CopyOptions { mode: Mode::Move, ..CopyOptions::default() };
    let (r, events) = run(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&folder)], &dest, &opts);
    let report = r.unwrap();
    assert!(report.ok(), "{report:?}");
    assert_eq!(report.copied, 0, "nothing needed copying again");
    assert_eq!(report.removed, 3, "but the originals were verified against the copies and removed");
    assert!(events.iter().any(|e| matches!(e, Event::Checking { .. })));
    assert!(!folder.exists());
}

#[test]
fn a_look_alike_with_different_content_is_never_taken_for_a_copy() {
    let (_tmp, src, dest) = setup();
    let file = src.join("small.csv");
    // Same size and date, different content: not the file that was copied.
    let data = fs::read(&file).unwrap();
    let mut other = data.clone();
    other[0] ^= 0xff;
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("small.csv"), &other).unwrap();
    let mtime = filetime::FileTime::from_last_modification_time(&fs::metadata(&file).unwrap());
    filetime::set_file_mtime(dest.join("small.csv"), mtime).unwrap();

    let opts = CopyOptions { mode: Mode::Move, ..CopyOptions::default() };
    let (r, _) = run(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&file)], &dest, &opts);
    let report = r.unwrap();
    assert_eq!(report.removed, 0);
    assert_eq!(fs::read(&file).unwrap(), data, "the original is untouched");
    assert_eq!(fs::read(dest.join("small.csv")).unwrap(), other);
}

#[test]
fn move_keeps_an_original_that_changed_after_it_was_copied() {
    let (_tmp, src, dest) = setup();
    let victim = src.join("small.csv");
    let opts = CopyOptions { mode: Mode::Move, ..CopyOptions::default() };
    let victim_path = s(&victim);
    let mut changed = false;
    let r = copy::copy(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&victim)], &s(&dest), &opts, &mut |e| {
        // After the copy is verified, before the original is checked and removed.
        if matches!(e, Event::Copied { .. }) && !changed {
            changed = true;
            fs::write(&victim_path, b"edited in the meantime").unwrap();
        }
    });
    let report = r.unwrap();
    assert_eq!(report.removed, 0);
    assert_eq!(report.kept.len(), 1, "{report:?}");
    assert_eq!(fs::read(&victim).unwrap(), b"edited in the meantime");
    assert!(dest.join("small.csv").exists());
}

#[test]
fn move_removes_emptied_folders_but_not_ones_with_leftovers() {
    let (_tmp, src, dest) = setup();
    write(&src.join("sub/.DS_Store"), b"junk that is never copied");
    let opts = CopyOptions { mode: Mode::Move, ..CopyOptions::default() };
    let (r, _) = run(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&src.join("sub"))], &dest, &opts);
    assert!(r.unwrap().ok());
    assert!(!src.join("sub/deeper").exists(), "emptied folders go");
    // "sub" still holds the skipped junk file, so it stays.
    assert!(src.join("sub/.DS_Store").exists());
    assert!(src.join("sub").exists());
}

#[test]
fn an_interrupted_copy_continues_from_its_part_file() {
    let (_tmp, src, dest) = setup();
    let big = src.join("sub/big_text.fastq");
    let data = fs::read(&big).unwrap();
    let target = dest.join("big_text.fastq");
    // A previous run got a third of the way.
    let cut = data.len() / 3;
    fs::write(part_string(&s(&target)), &data[..cut]).unwrap();

    let (r, events) = run(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&big)], &dest, &CopyOptions::default());
    let report = r.unwrap();
    assert!(report.ok(), "{report:?}");
    assert_eq!(fs::read(&target).unwrap(), data);
    assert!(!Path::new(&part_string(&s(&target))).exists());
    // Progress starts where the part file left off, not at zero.
    let first = events.iter().find_map(|e| if let Event::Progress { done, .. } = e { Some(*done) } else { None }).unwrap();
    assert_eq!(first, cut as u64);
}

#[test]
fn a_damaged_part_file_is_thrown_away_and_the_copy_retried() {
    let (_tmp, src, dest) = setup();
    let file = src.join("sub/random.bin");
    let data = fs::read(&file).unwrap();
    let target = dest.join("random.bin");
    let mut bad = data[..1000].to_vec();
    bad[500] ^= 0xff;
    fs::write(part_string(&s(&target)), &bad).unwrap();

    let (r, _) = run(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&file)], &dest, &CopyOptions::default());
    assert!(r.unwrap().ok());
    assert_eq!(fs::read(&target).unwrap(), data);
}

/// An endpoint that flips one byte of everything read through it.
struct Corrupting(LocalEnd);

impl Endpoint for Corrupting {
    fn machine(&self) -> String {
        "corrupting".into()
    }
    fn join(&self, dir: &str, name: &str) -> String {
        self.0.join(dir, name)
    }
    fn walk(&mut self, root: &str) -> Result<Walk> {
        self.0.walk(root)
    }
    fn stat(&mut self, path: &str) -> Result<Option<FileStat>> {
        self.0.stat(path)
    }
    fn mkdirs(&mut self, dirs: &[String]) -> Result<()> {
        self.0.mkdirs(dirs)
    }
    fn read(&mut self, path: &str, offset: u64, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<Digest> {
        let mut first = true;
        self.0.read(path, offset, &mut |chunk| {
            if first && !chunk.is_empty() {
                first = false;
                let mut bad = chunk.to_vec();
                bad[0] ^= 1;
                return sink(&bad);
            }
            sink(chunk)
        })
    }
    fn begin_write(&mut self, path: &str, size: u64, mtime_ns: i64, mode: u32, offset: u64) -> Result<()> {
        self.0.begin_write(path, size, mtime_ns, mode, offset)
    }
    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.0.write(data)
    }
    fn finish_write(&mut self, digest: Digest) -> Result<()> {
        self.0.finish_write(digest)
    }
    fn abort_write(&mut self) {
        self.0.abort_write()
    }
    fn remove(&mut self, path: &str) -> Result<()> {
        self.0.remove(path)
    }
}

#[test]
fn corruption_in_flight_is_caught_and_never_appears_under_the_real_name() {
    let (_tmp, src, dest) = setup();
    let file = src.join("small.csv");
    let (r, events) = run(&mut Corrupting(LocalEnd::new()), &mut LocalEnd::new(), &[s(&file)], &dest, &CopyOptions::default());
    let report = r.unwrap();
    assert_eq!(report.copied, 0);
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(report.failed[0].1.contains("checksum"), "{:?}", report.failed);
    assert!(events.iter().any(|e| matches!(e, Event::Failed { .. })));
    assert!(!dest.join("small.csv").exists(), "a wrong file must never take the real name");
    assert!(!Path::new(&part_string(&s(&dest.join("small.csv")))).exists(), "and no part file is left behind");
}

#[test]
fn corruption_is_caught_by_a_file_server_too() {
    let (tmp, src, _dest) = setup();
    let home = tmp.path().join("server_home");
    fs::create_dir_all(&home).unwrap();
    let mut server = Files::start(&home);
    let file = src.join("small.csv");
    let (r, _) = run(&mut Corrupting(LocalEnd::new()), server.end(), &[s(&file)], &home, &CopyOptions::default());
    let report = r.unwrap();
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(!home.join("small.csv").exists());
    // The server is still usable afterwards.
    let (r, _) = run(&mut LocalEnd::new(), server.end(), &[s(&file)], &home, &CopyOptions::default());
    assert!(r.unwrap().ok());
    assert_eq!(fs::read(home.join("small.csv")).unwrap(), fs::read(&file).unwrap());
}

#[test]
fn cancelling_stops_and_keeps_finished_files() {
    let (_tmp, src, dest) = setup();
    let flag = Arc::new(AtomicBool::new(false));
    let opts = CopyOptions { cancel: Some(flag.clone()), ..CopyOptions::default() };
    let mut copied = 0;
    let r = copy::copy(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&src)], &s(&dest), &opts, &mut |e| {
        if matches!(e, Event::Copied { .. }) {
            copied += 1;
            if copied == 2 {
                flag.store(true, Ordering::Relaxed);
            }
        }
    });
    let report = r.unwrap();
    assert!(report.cancelled);
    assert_eq!(report.copied, 2);
    assert!(!report.ok());
}

#[test]
fn a_folder_cannot_be_copied_into_itself() {
    let (_tmp, src, _dest) = setup();
    let inside = src.join("sub");
    let (r, _) = run(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&src)], &inside, &CopyOptions::default());
    assert!(matches!(r, Err(Error::Other(m)) if m.contains("into itself")));
}

#[test]
fn a_missing_destination_is_an_error_not_a_silent_mkdir() {
    let (_tmp, src, dest) = setup();
    let (r, _) = run(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&src.join("small.csv"))], &dest.join("nope"), &CopyOptions::default());
    assert!(matches!(r, Err(Error::NotFound(_))));
}

#[test]
fn file_server_remove_never_deletes_a_full_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("h");
    write(&home.join("d/file.txt"), b"x");
    let mut server = Files::start(&home);
    assert!(server.end().fs_remove(&s(&home.join("d"))).is_err(), "a folder with something in it stays");
    assert!(home.join("d/file.txt").exists());
    server.end().fs_remove(&s(&home.join("d/file.txt"))).unwrap();
    server.end().fs_remove(&s(&home.join("d"))).unwrap();
    assert!(!home.join("d").exists());
}

#[test]
fn keep_both_puts_a_folder_beside_the_existing_one() {
    let (_tmp, src, dest) = setup();
    let opts = CopyOptions { policy: LocalConflict::KeepBoth, ..CopyOptions::default() };
    let (r, _) = run(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&src)], &dest, &opts);
    assert!(r.unwrap().ok());
    assert_same(&snapshot(&dest.join("project")), &tree_snapshot(&src));
    assert!(!dest.join("project (2)").exists(), "nothing clashes the first time");

    // The same folder again: even though every file is identical, Keep both means a second copy.
    let (r, _) = run(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&src)], &dest, &opts);
    let report = r.unwrap();
    assert!(report.ok(), "{report:?}");
    assert_same(&snapshot(&dest.join("project (2)")), &tree_snapshot(&src));
    assert_same(&snapshot(&dest.join("project")), &tree_snapshot(&src));
    assert_eq!(report.kept_both.len(), 1);

    let (r, _) = run(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&src)], &dest, &opts);
    assert!(r.unwrap().ok());
    assert!(dest.join("project (3)/small.csv").exists());
}

#[test]
fn keep_both_on_a_move_back_leaves_both_and_removes_the_source() {
    // The case that was reported: copy to a server, then move back with Keep both.
    let (tmp, src, _dest) = setup();
    let home = tmp.path().join("server_home");
    fs::create_dir_all(&home).unwrap();
    let mut server = Files::start(&home);
    let (r, _) = run(&mut LocalEnd::new(), server.end(), &[s(&src)], &home, &CopyOptions::default());
    assert!(r.unwrap().ok());

    let parent = src.parent().unwrap().to_path_buf();
    let opts = CopyOptions { mode: Mode::Move, policy: LocalConflict::KeepBoth, ..CopyOptions::default() };
    let (r, _) = run(server.end(), &mut LocalEnd::new(), &[s(&home.join("project"))], &parent, &opts);
    let report = r.unwrap();
    assert!(report.ok(), "{report:?}");
    assert_same(&snapshot(&parent.join("project (2)")), &tree_snapshot(&src));
    assert!(src.join("small.csv").exists(), "the local original is untouched");
    assert!(!home.join("project").exists(), "the server copy was moved");
}

#[test]
fn a_continued_keep_both_fills_the_same_copy_instead_of_starting_another() {
    let (_tmp, src, dest) = setup();
    let opts = CopyOptions { policy: LocalConflict::KeepBoth, ..CopyOptions::default() };
    // The original is already there, and an earlier attempt made "project (2)" but stopped partway.
    let (r, _) = run(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&src)], &dest, &CopyOptions::default());
    assert!(r.unwrap().ok());
    write(&dest.join("project (2)/small.csv"), &fs::read(src.join("small.csv")).unwrap());
    filetime::set_file_mtime(
        dest.join("project (2)/small.csv"),
        filetime::FileTime::from_last_modification_time(&fs::metadata(src.join("small.csv")).unwrap()),
    )
    .unwrap();

    let again = CopyOptions { reuse: vec![s(&dest.join("project (2)"))], ..opts };
    let (r, _) = run(&mut LocalEnd::new(), &mut LocalEnd::new(), &[s(&src)], &dest, &again);
    assert!(r.unwrap().ok());
    assert_same(&snapshot(&dest.join("project (2)")), &tree_snapshot(&src));
    assert!(!dest.join("project (3)").exists());
}

#[test]
fn a_file_server_deletes_for_good_and_refuses_the_dangerous() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("h");
    write(&home.join("run42/sub/a.txt"), b"a");
    write(&home.join("keep.txt"), b"k");
    let mut server = Files::start(&home);
    // A refused request deletes nothing, even the items that were fine.
    assert!(server.end().fs_delete(&[s(&home.join("keep.txt")), "/".to_string()]).is_err());
    assert!(home.join("keep.txt").exists());
    let missing = server.end().fs_delete(&[s(&home.join("keep.txt")), s(&home.join("nope"))]).unwrap_err();
    assert!(missing.to_string().contains("isn't there"), "{missing}");
    assert!(home.join("keep.txt").exists());
    // A folder goes with everything in it.
    assert_eq!(server.end().fs_delete(&[s(&home.join("run42"))]).unwrap(), 1);
    assert!(!home.join("run42").exists());
    assert!(home.join("keep.txt").exists());
}

#[test]
fn a_file_server_trash_holds_items_until_restored_or_emptied() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("h");
    write(&home.join("run42/sub/a.txt"), b"a");
    write(&home.join("notes.txt"), b"n");
    let mut server = Files::start(&home);
    // Sending to the Trash moves the items; a refused request moves nothing.
    assert!(server.end().fs_trash(&[s(&home.join("notes.txt")), "/".to_string()]).is_err());
    assert!(home.join("notes.txt").exists());
    let report = server.end().fs_trash(&[s(&home.join("run42")), s(&home.join("notes.txt"))]).unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(report.trashed.len(), 2);
    assert!(!home.join("run42").exists() && !home.join("notes.txt").exists());

    let listed = server.end().fs_trash_list().unwrap();
    assert_eq!(listed.len(), 2);
    // Restore one; the other stays until the Trash is emptied.
    let back = listed.iter().find(|t| t.name == "run42").unwrap();
    server.end().fs_trash_restore(&back.id).unwrap();
    assert_eq!(fs::read(home.join("run42/sub/a.txt")).unwrap(), b"a");
    assert_eq!(server.end().fs_trash_list().unwrap().len(), 1);
    let kept = server.end().fs_trash_list().unwrap().remove(0);
    assert_eq!(server.end().fs_trash_empty(None).unwrap(), 1);
    assert!(!Path::new(&kept.stored).exists(), "emptying deletes for good");
    assert!(server.end().fs_trash_list().unwrap().is_empty());
    assert!(home.join("run42").exists(), "what was restored is untouched");
}
