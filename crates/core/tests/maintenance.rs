//! PAR2 protection, scrub and repair, damage reporting, info, and trash purge.

use std::fs;
use std::path::Path;

use archive_core::catalog::{self, PackState};
use archive_core::maint::{self, Protection};
use archive_core::par2::Par2;
use archive_core::retrieve::{self, RetrieveOptions};
use archive_core::seekable::CompressOptions;
use archive_core::send::{self, Mode, SendOptions};
use archive_core::store::{Config, Store};
use archive_core::{Archive, VPath};

mod common;

fn text(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2654435761) | 1;
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            b"ACGTN\t\n0123"[(x >> 60) as usize % 11]
        })
        .collect()
}

fn par2() -> Option<Par2> {
    match Par2::find(None) {
        Ok(p) => Some(p),
        Err(_) => {
            eprintln!("skipping: par2 not installed");
            None
        }
    }
}

fn setup(dir: &Path) -> Store {
    let src = dir.join("proj");
    fs::create_dir_all(src.join("raw")).unwrap();
    fs::write(src.join("raw/a.fastq"), text(3 << 20, 1)).unwrap();
    fs::write(src.join("raw/b.fastq"), text(3 << 20, 2)).unwrap();
    fs::write(src.join("raw/b_copy.fastq"), text(3 << 20, 2)).unwrap();
    fs::write(src.join("notes.txt"), text(10_000, 3)).unwrap();
    let mut store = Store::init(&dir.join("arc"), Config { pack_target_bytes: 1 << 20, ..Config::default() }).unwrap();
    let o = SendOptions {
        mode: Mode::Copy,
        compress: CompressOptions { level: 3, frame_size: 256 << 10, threads: 2 },
        ..SendOptions::default()
    };
    let r = send::send(&mut store, &[src], &VPath::root(), &o, &mut |_| {}).unwrap();
    assert!(r.ok());
    store
}

#[test]
fn protect_scrub_repair_and_damage() {
    let Some(par2) = par2() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let mut store = setup(tmp.path());

    let info = maint::info(&store, &VPath::parse("/proj").unwrap()).unwrap();
    assert_eq!(info.protection, Protection::Pending);
    assert_eq!(info.files, 4);
    assert_eq!(info.duplicate_bytes, 3 << 20);
    assert!(info.stored_bytes < info.original_bytes - info.duplicate_bytes, "{info:?}");

    let res = maint::protect(&store, &par2, None, &mut |_| {}).unwrap();
    assert!(res.iter().all(|r| r.outcome == "protected"), "{res:?}");
    let info = maint::info(&store, &VPath::parse("/proj").unwrap()).unwrap();
    assert_eq!(info.protection, Protection::Protected);
    assert!(info.last_checked.is_some());

    // Light damage in the pack holding a.fastq: scrub repairs it.
    let a = store.stat(&VPath::parse("/proj/raw/a.fastq").unwrap()).unwrap().unwrap();
    let loc = catalog::location(store.conn(), a.content_id.unwrap()).unwrap();
    let path = store.pack_path(&loc.pack_name);
    let original = fs::read(&path).unwrap();
    let mut bytes = original.clone();
    for i in 0..2000 {
        bytes[(loc.data_off + 1000 + i) as usize] ^= 0xA5;
    }
    fs::write(&path, &bytes).unwrap();
    let res = maint::scrub(&store, &par2, maint::ScrubOptions::default(), &mut |_| {}).unwrap();
    let r = res.iter().find(|r| r.pack == loc.pack_name).unwrap();
    assert_eq!(r.outcome, "damage found and repaired");
    assert_eq!(fs::read(&path).unwrap(), original, "repair must restore the exact bytes");
    assert!(!path.with_extension("tar.1").exists(), "par2's backup copy is cleaned up");

    // Retrieval after repair is byte-exact.
    let out = tmp.path().join("out");
    let rr =
        retrieve::retrieve(&mut { store }, &VPath::parse("/proj/raw/a.fastq").unwrap(), &out, &RetrieveOptions::default(), &mut |_| {})
            .unwrap();
    assert!(rr.ok());
    assert_eq!(fs::read(out.join("a.fastq")).unwrap(), text(3 << 20, 1));
}

#[test]
fn damage_beyond_recovery_is_reported() {
    let Some(par2) = par2() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let mut store = setup(tmp.path());
    maint::protect(&store, &par2, None, &mut |_| {}).unwrap();
    let a = store.stat(&VPath::parse("/proj/raw/a.fastq").unwrap()).unwrap().unwrap();
    let loc = catalog::location(store.conn(), a.content_id.unwrap()).unwrap();
    let path = store.pack_path(&loc.pack_name);
    let mut bytes = fs::read(&path).unwrap();
    let n = bytes.len();
    for b in &mut bytes[n / 4..n / 2] {
        *b = !*b;
    }
    fs::write(&path, &bytes).unwrap();
    maint::scrub(&store, &par2, maint::ScrubOptions::default(), &mut |_| {}).unwrap();
    assert_eq!(catalog::pack(store.conn(), loc.pack_id).unwrap().state, PackState::Damaged);
    let info = maint::info(&store, &VPath::parse("/proj").unwrap()).unwrap();
    assert_eq!(info.protection, Protection::Damaged);
}

#[test]
fn purging_a_project_frees_its_packs_and_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = setup(tmp.path());
    let first: Vec<String> = catalog::packs(store.conn()).unwrap().into_iter().map(|p| p.name).collect();

    // The same data as a second project is stored again: projects never share data.
    let o = SendOptions { compress: CompressOptions { level: 3, frame_size: 256 << 10, threads: 2 }, ..SendOptions::default() };
    let r = send::send(&mut store, &[tmp.path().join("proj")], &VPath::parse("/Other").unwrap(), &o, &mut |_| {}).unwrap();
    assert!(r.ok());
    assert_eq!((r.stored, r.deduplicated), (3, 1), "{r:?}");

    // Files inside a project can't be trashed on their own; the project can.
    assert!(store.trash(&VPath::parse("/proj/raw/b_copy.fastq").unwrap()).is_err());
    store.trash(&VPath::parse("/proj").unwrap()).unwrap();
    let r = maint::purge(&mut store, Some(1)).unwrap();
    assert_eq!(r.purged_items, 0, "items trashed just now aren't a day old yet");
    // Older than 0 days: anything trashed before this second (so wait one out).
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let r = maint::purge(&mut store, Some(0)).unwrap();
    assert_eq!(r.purged_items, 1);
    let mut deleted = r.packs_deleted.clone();
    deleted.sort();
    let mut expected = first.clone();
    expected.sort();
    assert_eq!(deleted, expected, "exactly the first project's packs are freed");
    for name in &r.packs_deleted {
        assert!(!store.pack_path(name).exists());
    }
    assert_eq!(store.stat(&VPath::root()).unwrap().unwrap().files, 4);
    let out = tmp.path().join("out");
    let g = retrieve::retrieve(&mut store, &VPath::parse("/Other/proj").unwrap(), &out, &RetrieveOptions::default(), &mut |_| {}).unwrap();
    assert!(g.ok() && g.files == 4, "{g:?}");
}

#[test]
fn catalog_rebuilds_from_packs_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("project");
    common::make_tree(&src);
    let before = common::snapshot(&src);
    let root = tmp.path().join("arc");
    let mut store = Store::init(&root, Config { pack_target_bytes: 1 << 20, ..Config::default() }).unwrap();
    let o = SendOptions {
        mode: Mode::Copy,
        compress: CompressOptions { level: 3, frame_size: 64 << 10, threads: 2 },
        solid_block_max: 256 << 10,
        ..SendOptions::default()
    };
    send::send(&mut store, &[src.clone()], &VPath::parse("/A").unwrap(), &o, &mut |_| {}).unwrap();
    // A second project with the same data, later moved and added to.
    send::send(&mut store, &[src.clone()], &VPath::parse("/B").unwrap(), &o, &mut |_| {}).unwrap();
    store.mkdir_p(&VPath::parse("/Archive/2024").unwrap()).unwrap();
    store.rename(&VPath::parse("/B/project").unwrap(), &VPath::parse("/Archive/2024/Bproj").unwrap()).unwrap();
    let extra = tmp.path().join("extra.txt");
    fs::write(&extra, b"added later").unwrap();
    let r = send::send(&mut store, &[extra], &VPath::parse("/Archive/2024/Bproj").unwrap(), &o, &mut |_| {}).unwrap();
    assert_eq!(r.stored, 1);
    let want: Vec<_> =
        store.walk(&VPath::root()).unwrap().into_iter().map(|(p, e)| (p, e.kind, e.size, e.files, e.sha256, e.target)).collect();
    drop(store);

    let rebuilt = tmp.path().join("rebuilt.db");
    let report = archive_core::rebuild::rebuild(&root, &rebuilt).unwrap();
    assert!(report.skipped_packs.is_empty() && report.missing.is_empty(), "{report:?}");

    // Swap the rebuilt catalog in and compare the whole tree, then read everything back.
    fs::remove_file(root.join("catalog.db")).unwrap();
    for ext in ["catalog.db-wal", "catalog.db-shm"] {
        let _ = fs::remove_file(root.join(ext));
    }
    fs::copy(&rebuilt, root.join("catalog.db")).unwrap();
    let mut store = Store::open(&root).unwrap();
    let got: Vec<_> =
        store.walk(&VPath::root()).unwrap().into_iter().map(|(p, e)| (p, e.kind, e.size, e.files, e.sha256, e.target)).collect();
    assert_eq!(got, want);
    // Projects come back as projects, the moved one where its newest pack says.
    for p in ["/A/project", "/Archive/2024/Bproj"] {
        assert!(store.stat(&VPath::parse(p).unwrap()).unwrap().unwrap().is_project, "{p}");
    }
    let out = tmp.path().join("out");
    let r = retrieve::retrieve(&mut store, &VPath::parse("/Archive/2024/Bproj").unwrap(), &out, &RetrieveOptions::default(), &mut |_| {})
        .unwrap();
    assert!(r.ok(), "{:?}", r.failed);
    let mut got = common::snapshot(&out.join("Bproj"));
    assert!(matches!(got.remove("extra.txt"), Some(common::Node::File(ref b, _)) if b == b"added later"));
    common::assert_same(&got, &before);
}
