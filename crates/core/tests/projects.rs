//! Archived projects: what can and can't change, loose files, INDEX.tsv, and
//! recovering one project from its own packs with standard tools.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;

mod common;
use common::*;

use archive_core::api::{Archive, FileMeta, PutFile};
use archive_core::catalog::{self, NodeKind};
use archive_core::hash::Digest;
use archive_core::seekable::{CompressOptions, CompressingReader};
use archive_core::send::{self, SendOptions};
use archive_core::store::{Config, Store};
use archive_core::{Conflict, VPath};

fn opts() -> SendOptions {
    SendOptions { compress: CompressOptions { level: 3, frame_size: 64 << 10, threads: 2 }, solid_block_max: 256 << 10, ..SendOptions::default() }
}

fn p(s: &str) -> VPath {
    VPath::parse(s).unwrap()
}

fn have_tools() -> bool {
    let have = |c: &str| Command::new(c).arg("--version").output().is_ok();
    if !have("zstd") || !have("tar") {
        eprintln!("skipping: zstd or tar not installed");
        return false;
    }
    true
}

/// An archive holding the test tree as the project /Lab/2024/Dairy.
fn setup() -> (tempfile::TempDir, Store, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("Dairy");
    make_tree(&src);
    let mut store = Store::init(&tmp.path().join("arc"), Config { pack_target_bytes: 1 << 20, ..Config::default() }).unwrap();
    store.mkdir_p(&p("/Lab/2024")).unwrap();
    let r = send::send(&mut store, &[src.clone()], &p("/Lab/2024"), &opts(), &mut |_| {}).unwrap();
    assert!(r.ok(), "{r:?}");
    (tmp, store, src)
}

fn small_project(tmp: &std::path::Path, store: &mut Store, name: &str, dest: &str) {
    let dir = tmp.join(name);
    write(&dir.join("readme.txt"), name.as_bytes());
    let r = send::send(store, &[dir], &p(dest), &opts(), &mut |_| {}).unwrap();
    assert!(r.ok(), "{r:?}");
}

#[test]
fn projects_are_frozen_but_can_be_reorganized() {
    let (tmp, mut store, _src) = setup();
    let top = store.stat(&p("/Lab/2024/Dairy")).unwrap().unwrap();
    assert!(top.is_project && !top.in_project);
    assert!(store.stat(&p("/Lab/2024/Dairy/sub")).unwrap().unwrap().in_project);
    assert!(!store.stat(&p("/Lab/2024")).unwrap().unwrap().in_project);

    // Nothing inside a project can be renamed, moved, trashed, or created.
    assert!(store.rename(&p("/Lab/2024/Dairy/small.csv"), &p("/Lab/2024/Dairy/renamed.csv")).is_err());
    assert!(store.rename(&p("/Lab/2024/Dairy/sub"), &p("/Lab/sub")).is_err());
    assert!(store.trash(&p("/Lab/2024/Dairy/sub/random.bin")).is_err());
    assert!(store.create_folder(&p("/Lab/2024/Dairy/new")).is_err());

    // The project as a whole can be renamed and moved, and organizing folders too.
    store.rename(&p("/Lab/2024/Dairy"), &p("/Lab/2024/Dairy study")).unwrap();
    store.mkdir_p(&p("/Old")).unwrap();
    store.rename(&p("/Lab/2024"), &p("/Old/2024")).unwrap();
    assert!(store.stat(&p("/Old/2024/Dairy study/small.csv")).unwrap().is_some());

    // Never into another project, though.
    small_project(tmp.path(), &mut store, "Other", "/Old");
    assert!(store.rename(&p("/Old/2024/Dairy study"), &p("/Old/Other/Dairy study")).is_err());
    assert!(store.rename(&p("/Old/2024"), &p("/Old/Other/2024")).is_err());

    // A whole project goes to the trash and back, but not into another project.
    let id = store.trash(&p("/Old/2024/Dairy study")).unwrap();
    assert!(store.restore(id, Some(&p("/Old/Other/Dairy study"))).is_err());
    assert_eq!(store.restore(id, None).unwrap(), p("/Old/2024/Dairy study"));
    assert!(store.stat(&p("/Old/2024/Dairy study")).unwrap().unwrap().is_project);

    // An organizing folder holding projects can be trashed as a whole.
    store.trash(&p("/Old")).unwrap();
    assert_eq!(store.stat(&VPath::root()).unwrap().unwrap().files, 0);
}

#[test]
fn files_go_into_projects() {
    let (tmp, mut store, src) = setup();
    let loose = tmp.path().join("loose");
    write(&loose.join("a.txt"), b"a");
    write(&loose.join("b.txt"), b"b");
    let files = vec![loose.join("a.txt"), loose.join("b.txt")];

    // Loose files outside every project are refused, one by one.
    let r = send::send(&mut store, &files, &p("/Lab"), &opts(), &mut |_| {}).unwrap();
    assert_eq!(r.failed.len(), 2, "{r:?}");
    assert!(r.failed[0].1.contains("isn't inside a project"), "{r:?}");
    assert!(store.stat(&p("/Lab/a.txt")).unwrap().is_none());

    // Wrapped in a new project, they go in.
    let mut o = opts();
    o.new_project = Some("Notes 2024".into());
    let r = send::send(&mut store, &files, &p("/Lab"), &o, &mut |_| {}).unwrap();
    assert!(r.ok() && r.stored == 2, "{r:?}");
    assert!(store.stat(&p("/Lab/Notes 2024")).unwrap().unwrap().is_project);
    assert!(store.stat(&p("/Lab/Notes 2024/a.txt")).unwrap().unwrap().in_project);

    // Adding to a project: new files and folders go in, a changed file is
    // skipped (or kept alongside), and nothing is replaced.
    fs::write(src.join("small.csv"), b"changed").unwrap();
    write(&src.join("later/added.txt"), b"added later");
    let r = send::send(&mut store, &[src.join("small.csv"), src.join("later")], &p("/Lab/2024/Dairy"), &opts(), &mut |_| {}).unwrap();
    assert_eq!((r.stored, r.skipped.len()), (1, 1), "{r:?}");
    let later = store.stat(&p("/Lab/2024/Dairy/later")).unwrap().unwrap();
    assert!(later.in_project && !later.is_project, "a folder sent into a project is part of it");
    let mut o = opts();
    o.policy = Conflict::KeepBoth;
    let r = send::send(&mut store, &[src.join("small.csv")], &p("/Lab/2024/Dairy"), &o, &mut |_| {}).unwrap();
    assert_eq!(r.stored, 1, "{r:?}");
    assert!(store.stat(&p("/Lab/2024/Dairy/small (2).csv")).unwrap().is_some());

    // Space used: compressed, and a duplicate counts once.
    let top = store.stat(&p("/Lab/2024/Dairy")).unwrap().unwrap();
    assert!(top.stored > 0 && top.stored < top.size, "{} of {}", top.stored, top.size);
    let (a, b) = (store.stat(&p("/Lab/2024/Dairy/dup_a.dat")).unwrap().unwrap(), store.stat(&p("/Lab/2024/Dairy/dup_b.dat")).unwrap().unwrap());
    assert!(a.stored > 0 && b.stored == 0);
    catalog::recount_stored(store.conn()).unwrap();
    assert_eq!(store.stat(&p("/Lab/2024/Dairy")).unwrap().unwrap().stored, top.stored, "totals kept as files arrive match a recount");

    // Everything a project holds is in its own packs.
    let project = store.stat(&p("/Lab/2024/Dairy")).unwrap().unwrap().project;
    let other = store.stat(&p("/Lab/Notes 2024")).unwrap().unwrap().project;
    let packs = catalog::packs(store.conn()).unwrap();
    assert!(packs.iter().all(|k| k.project == project || k.project == other));
    assert!(packs.iter().any(|k| k.project == other));
}

#[test]
fn only_the_same_transfer_can_replace_a_file() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::init(&tmp.path().join("arc"), Config::default()).unwrap();
    catalog::create_project(store.conn(), catalog::ROOT_ID, "P", 0, 0o755).unwrap();
    let put = |store: &mut Store, job: &str, data: &[u8], policy: Conflict| {
        let req = PutFile { dest: p("/P/f.bin"), meta: FileMeta { size: data.len() as u64, mtime_ns: 0, mode: 0o644 }, policy };
        let mut payload = CompressingReader::new(data, CompressOptions::default()).unwrap();
        store.put_file(job, &req, &mut payload)
    };
    let first = text(2 << 20, 1);
    let second = text(2 << 20, 2);
    put(&mut store, "j1", &first, Conflict::Skip).unwrap();
    // Still the same transfer (its pack is open): a changed file may be sent again.
    put(&mut store, "j1", &second, Conflict::Replace).unwrap();
    store.finish_job("j1").unwrap();
    // Afterwards, and from any other transfer, never.
    assert!(put(&mut store, "j1", &first, Conflict::Replace).is_err());
    assert!(put(&mut store, "j2", &first, Conflict::Replace).is_err());
    let mut got = Vec::new();
    store.read_file(&p("/P/f.bin")).unwrap().1.read_to_end(&mut got).unwrap();
    assert_eq!(got, second);

    // Recovery with standard tools gets the newer copy, though both are in the pack.
    if !have_tools() {
        return;
    }
    let out = tmp.path().join("out");
    let script = tmp.path().join("recover.sh");
    fs::write(&script, archive_core::recovery::RECOVER_SH).unwrap();
    for pack in catalog::packs(store.conn()).unwrap() {
        let st = Command::new("sh").arg(&script).arg(store.pack_path(&pack.name)).arg(&out).output().unwrap();
        assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
    }
    assert_eq!(fs::read(out.join("P/f.bin")).unwrap(), second);
}

#[test]
fn a_project_recovers_from_its_own_packs() {
    if !have_tools() {
        return;
    }
    let (tmp, mut store, src) = setup();
    let before = snapshot(&src);
    small_project(tmp.path(), &mut store, "Other", "/Lab");
    let project = store.stat(&p("/Lab/2024/Dairy")).unwrap().unwrap().project;

    let out = tmp.path().join("out");
    let script = tmp.path().join("recover.sh");
    fs::write(&script, archive_core::recovery::RECOVER_SH).unwrap();
    let mut packs: Vec<_> = catalog::packs(store.conn()).unwrap().into_iter().filter(|k| k.project == project).collect();
    packs.sort_by_key(|k| k.id);
    assert!(packs.len() > 1);
    for pack in packs {
        let head = Command::new("tar").arg("-xOf").arg(store.pack_path(&pack.name)).arg(".archive/PACK.json").output().unwrap();
        let head: serde_json::Value = serde_json::from_slice(&head.stdout).unwrap();
        assert_eq!(head["project"]["path"], "Lab/2024/Dairy", "each pack names its project");
        let st = Command::new("sh").arg(&script).arg(store.pack_path(&pack.name)).arg(&out).output().unwrap();
        assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
    }
    assert!(!out.join("Lab/Other").exists());
    assert_same(&snapshot(&out.join("Lab/2024/Dairy")), &before);
}

#[test]
fn every_file_is_in_the_index_and_extracts_with_standard_tools() {
    let (_tmp, mut store, _src) = setup();
    // Moved after it was archived: the index has today's path and the stored one.
    store.rename(&p("/Lab/2024/Dairy"), &p("/Lab/2024/Dairy study")).unwrap();
    let index = archive_core::worker::write_index(&store).unwrap();
    let text = fs::read_to_string(&index).unwrap();
    let mut lines = text.lines().skip_while(|l| l.starts_with('#'));
    assert_eq!(lines.next(), Some("path\tsize\tsha256\tpack\tstart\tlength\toffset\tmember\tstored_as"));
    let rows: HashMap<String, Vec<String>> = lines
        .map(|l| {
            let cols: Vec<String> = l.split('\t').map(str::to_string).collect();
            assert_eq!(cols.len(), 9, "{l}");
            (cols[0].clone(), cols)
        })
        .collect();
    let files: Vec<String> = store
        .walk(&VPath::root())
        .unwrap()
        .into_iter()
        .filter(|(_, e)| e.kind == NodeKind::File)
        .map(|(rel, _)| format!("/{rel}"))
        .collect();
    assert_eq!(rows.len(), files.len());
    let big = &rows["/Lab/2024/Dairy study/sub/big_text.fastq"];
    assert_eq!(big[8], "Lab/2024/Dairy/sub/big_text.fastq");

    if !have_tools() {
        return;
    }
    let extract = |cmd: &str, cols: &[String]| {
        let pack = store.pack_path(&cols[3]);
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd).arg("sh").arg(&pack);
        c.args(&cols[4..]).arg(&cols[1]);
        c.output().unwrap()
    };
    for f in &files {
        let cols = &rows[f];
        if cols[1] == "0" {
            assert_eq!(cols[2], Digest::of(b"").to_hex(), "{f}");
            continue;
        }
        // The command RECOVERY.txt gives ($2 START, $3 LENGTH, $4 OFFSET, $7 SIZE).
        let out = extract(r#"tail -c +$(($2 + 1)) "$1" | head -c "$3" | zstd -dc | tail -c +$(($4 + 1)) | head -c "$7""#, cols);
        assert!(out.status.success(), "{f}: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(Digest::of(&out.stdout).to_hex(), cols[2], "{f}");
        // And by name with tar ($5 MEMBER, $6 STORED_AS), for plain names.
        if f.is_ascii() {
            let by_name = if cols[7].starts_with(".archive/solid-") {
                r#"tar -xOf "$1" "$5" | zstd -dc | tar -xOf - "$6""#
            } else {
                r#"tar -xOf "$1" "$5" | zstd -dc"#
            };
            let out = extract(by_name, cols);
            assert!(out.status.success(), "{f}: {}", String::from_utf8_lossy(&out.stderr));
            assert_eq!(Digest::of(&out.stdout).to_hex(), cols[2], "{f}");
        }
    }
}
