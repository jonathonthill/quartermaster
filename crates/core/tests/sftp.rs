//! The SFTP client against a real OpenSSH `sftp-server` (run directly, without ssh), in temporary
//! folders: copying both ways, large pipelined files, resuming, corruption, moving, listing, the
//! Trash, and deleting.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::*;

use archive_core::copy::{self, CopyOptions, Endpoint, LocalEnd, part_string};
use archive_core::retrieve::LocalConflict;
use archive_core::send::Mode;
use archive_core::sftp::Sftp;

fn server_binary() -> Option<PathBuf> {
    ["/usr/libexec/sftp-server", "/usr/lib/openssh/sftp-server", "/usr/lib/ssh/sftp-server"].iter().map(PathBuf::from).find(|p| p.exists())
}

/// An SFTP session to a local sftp-server whose home (starting folder) is `home`.
fn open(home: &Path) -> Option<Sftp> {
    let bin = server_binary()?;
    let mut cmd = Command::new(bin);
    cmd.current_dir(home);
    Some(Sftp::spawn(cmd, "sftp-test").unwrap())
}

macro_rules! sftp_or_skip {
    ($home:expr) => {
        match open($home) {
            Some(s) => s,
            None => {
                eprintln!("no sftp-server here; skipping");
                return;
            }
        }
    };
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

fn tree_snapshot(root: &Path) -> std::collections::BTreeMap<String, Node> {
    let mut snap = snapshot(root);
    snap.retain(|_, n| !matches!(n, Node::Link(_)));
    snap
}

fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    // Resolve /var -> /private/var on macOS, so paths match what the server reports.
    let base = fs::canonicalize(tmp.path()).unwrap();
    let src = base.join("project");
    make_tree(&src);
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    (tmp, src, home)
}

#[test]
fn copies_to_and_from_an_sftp_server_exactly() {
    let (_t, src, home) = setup();
    let mut sftp = sftp_or_skip!(&home);
    assert_eq!(sftp.home(), s(&home));

    let report = copy::copy(&mut LocalEnd::new(), &mut sftp, &[s(&src)], &s(&home), &CopyOptions::default(), &mut |_| {}).unwrap();
    assert!(report.ok(), "{report:?}");
    assert!(report.size_only, "an SFTP server is checked by size, and the report says so");
    assert_same(&snapshot(&home.join("project")), &tree_snapshot(&src));
    // Dates are kept (to the second, which is what SFTP carries).
    let a = fs::metadata(src.join("dup_b.dat")).unwrap().modified().unwrap();
    let b = fs::metadata(home.join("project/dup_b.dat")).unwrap().modified().unwrap();
    assert_eq!(a.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(), b.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());

    let back = home.parent().unwrap().join("back");
    fs::create_dir_all(&back).unwrap();
    let report =
        copy::copy(&mut sftp, &mut LocalEnd::new(), &[s(&home.join("project"))], &s(&back), &CopyOptions::default(), &mut |_| {}).unwrap();
    assert!(report.ok(), "{report:?}");
    assert_same(&snapshot(&back.join("project")), &tree_snapshot(&src));
}

#[test]
fn large_files_go_through_the_pipeline_intact() {
    let (_t, _src, home) = setup();
    let mut sftp = sftp_or_skip!(&home);
    let big = home.parent().unwrap().join("big.bin");
    let data = noise(9 << 20, 11);
    fs::write(&big, &data).unwrap();
    let report = copy::copy(&mut LocalEnd::new(), &mut sftp, &[s(&big)], &s(&home), &CopyOptions::default(), &mut |_| {}).unwrap();
    assert!(report.ok(), "{report:?}");
    assert_eq!(fs::read(home.join("big.bin")).unwrap(), data);
    // And back down, through the read pipeline.
    let down = home.parent().unwrap().join("down");
    fs::create_dir_all(&down).unwrap();
    let report =
        copy::copy(&mut sftp, &mut LocalEnd::new(), &[s(&home.join("big.bin"))], &s(&down), &CopyOptions::default(), &mut |_| {}).unwrap();
    assert!(report.ok(), "{report:?}");
    assert_eq!(fs::read(down.join("big.bin")).unwrap(), data);
}

#[test]
fn an_interrupted_upload_continues_from_its_part_file() {
    let (_t, _src, home) = setup();
    let mut sftp = sftp_or_skip!(&home);
    let file = home.parent().unwrap().join("data.bin");
    let data = noise(3 << 20, 5);
    fs::write(&file, &data).unwrap();
    let target = home.join("data.bin");
    fs::write(part_string(&s(&target)), &data[..1_000_000]).unwrap();
    let mut first = None;
    let report = copy::copy(&mut LocalEnd::new(), &mut sftp, &[s(&file)], &s(&home), &CopyOptions::default(), &mut |e| {
        if let copy::Event::Progress { done, .. } = e {
            first.get_or_insert(*done);
        }
    })
    .unwrap();
    assert!(report.ok(), "{report:?}");
    assert_eq!(first, Some(1_000_000), "it started where the part file ended");
    assert_eq!(fs::read(&target).unwrap(), data);
    assert!(!Path::new(&part_string(&s(&target))).exists());
}

#[test]
fn a_moved_folder_leaves_the_sftp_server_only_once_everything_arrived() {
    let (_t, src, home) = setup();
    let mut sftp = sftp_or_skip!(&home);
    copy::copy(&mut LocalEnd::new(), &mut sftp, &[s(&src.join("sub"))], &s(&home), &CopyOptions::default(), &mut |_| {}).unwrap();
    let dest = home.parent().unwrap().join("dest");
    fs::create_dir_all(&dest).unwrap();
    let opts = CopyOptions { mode: Mode::Move, ..CopyOptions::default() };
    let report = copy::copy(&mut sftp, &mut LocalEnd::new(), &[s(&home.join("sub"))], &s(&dest), &opts, &mut |_| {}).unwrap();
    assert!(report.ok(), "{report:?}");
    assert!(report.removed >= 3);
    assert!(dest.join("sub/random.bin").exists());
    assert!(!home.join("sub/random.bin").exists());
}

#[test]
fn keep_both_and_read_back_work_on_sftp() {
    let (_t, src, home) = setup();
    let mut sftp = sftp_or_skip!(&home);
    sftp.read_back = true;
    let one = [s(&src.join("small.csv"))];
    copy::copy(&mut LocalEnd::new(), &mut sftp, &one, &s(&home), &CopyOptions::default(), &mut |_| {}).unwrap();
    let opts = CopyOptions { policy: LocalConflict::KeepBoth, ..CopyOptions::default() };
    let report = copy::copy(&mut LocalEnd::new(), &mut sftp, &one, &s(&home), &opts, &mut |_| {}).unwrap();
    assert!(report.ok(), "{report:?}");
    assert!(!report.size_only, "with read-back on, uploads are checked by checksum");
    assert_eq!(fs::read(home.join("small (2).csv")).unwrap(), fs::read(src.join("small.csv")).unwrap());
}

#[test]
fn listing_shows_the_folder_from_the_home_folder_down() {
    let (_t, src, home) = setup();
    let mut sftp = sftp_or_skip!(&home);
    copy::copy(&mut LocalEnd::new(), &mut sftp, &[s(&src)], &s(&home), &CopyOptions::default(), &mut |_| {}).unwrap();
    let l = sftp.list(Some("~/project"), false).unwrap();
    assert_eq!(l.path, s(&home.join("project")));
    assert_eq!(l.crumbs.first().map(|c| c.name.as_str()), Some("Home"));
    let names: Vec<&str> = l.items.iter().map(|i| i.name.as_str()).collect();
    assert!(names.contains(&"sub") && names.contains(&"small.csv"));
    assert!(!names.iter().any(|n| n.starts_with('.')), "hidden files are hidden");
    assert_eq!(l.items[0].kind, "dir", "folders first");
    let m = sftp.measure(&[s(&home.join("project/sub"))]).unwrap();
    assert!(m.files >= 3 && m.bytes > 5_000_000);
    let found = sftp.search(&s(&home), "notes", false, 50, &|| false).unwrap();
    assert!(found.hits.iter().any(|h| h.item.name == "notes.md"));
}

#[test]
fn the_trash_on_an_sftp_server_holds_items_until_restored_or_emptied() {
    let (_t, _src, home) = setup();
    let mut sftp = sftp_or_skip!(&home);
    write(&home.join("data/run42/sub/a.txt"), b"a");
    write(&home.join("data/notes.txt"), b"n");
    // Refused requests move nothing.
    assert!(sftp.trash(&[s(&home.join("data/notes.txt")), s(&home)]).is_err());
    assert!(sftp.trash(&[s(&home.join("data/notes.txt")), "/".into()]).is_err());
    assert!(home.join("data/notes.txt").exists());

    let report = sftp.trash(&[s(&home.join("data/run42")), s(&home.join("data/notes.txt"))]).unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    assert!(!home.join("data/run42").exists());
    let listed = sftp.trash_list().unwrap();
    assert_eq!(listed.len(), 2);
    let run = listed.iter().find(|t| t.name == "run42").unwrap().clone();
    // Something in the Trash isn't trashed again.
    assert!(sftp.trash(&[run.stored.clone()]).is_err());
    sftp.trash_restore(&run.id).unwrap();
    assert_eq!(fs::read(home.join("data/run42/sub/a.txt")).unwrap(), b"a");
    let kept = sftp.trash_list().unwrap().remove(0);
    assert_eq!(sftp.trash_empty(None).unwrap(), 1);
    assert!(!Path::new(&kept.stored).exists(), "emptying deletes for good");
    assert!(sftp.trash_list().unwrap().is_empty());
}

#[test]
fn deleting_on_sftp_removes_everything_inside_but_refuses_the_dangerous() {
    let (_t, _src, home) = setup();
    let mut sftp = sftp_or_skip!(&home);
    write(&home.join("data/run42/sub/deep/a.txt"), b"a");
    write(&home.join("data/keep.txt"), b"k");
    assert!(sftp.delete(&[s(&home.join("data/keep.txt")), s(&home)]).is_err());
    assert!(home.join("data/keep.txt").exists(), "a refused request deletes nothing");
    assert_eq!(sftp.delete(&[s(&home.join("data/run42"))]).unwrap(), 1);
    assert!(!home.join("data/run42").exists());
    assert!(home.join("data/keep.txt").exists());
}

#[test]
fn a_missing_sftp_server_path_reads_as_missing() {
    let (_t, _src, home) = setup();
    let mut sftp = sftp_or_skip!(&home);
    assert!(sftp.stat(&s(&home.join("nope"))).unwrap().is_none());
    assert!(matches!(sftp.list(Some(&s(&home.join("nope"))), false), Err(archive_core::Error::NotFound(_))));
}
