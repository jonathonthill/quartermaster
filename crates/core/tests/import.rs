//! Importing move2storage bundles, built here the way that tool builds them.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::*;

use archive_core::api::Archive;
use archive_core::import::{self, ImportOptions};
use archive_core::retrieve::{self, RetrieveOptions};
use archive_core::store::{Config, Store};
use archive_core::VPath;

/// Python that builds `<part>.bundle.tar` from a folder of files as the
/// move2storage server did: files compressed with `zstd -3` as `<rel>.zst`,
/// tarred under the folder's name, then wrapped with a checksum and manifest.
const BUILD: &str = r#"
import hashlib, json, os, subprocess, sys, tarfile
src, out, part, name = sys.argv[1:5]
def sha(p):
    h = hashlib.sha256()
    with open(p, 'rb') as f:
        for b in iter(lambda: f.read(1 << 20), b''): h.update(b)
    return h.hexdigest()
work = os.path.join(out, part + '.work'); tree = os.path.join(work, 'tree', name)
files, dirs = [], ['.']
for d, ds, fs in os.walk(src):
    rel = os.path.relpath(d, src)
    for x in ds: dirs.append(os.path.normpath(os.path.join(rel, x)))
    for x in fs:
        p = os.path.join(d, x); r = os.path.normpath(os.path.join(rel, x)); dst = os.path.join(tree, r + '.zst')
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        subprocess.run(['zstd', '-q', '-3', '-f', p, '-o', dst], check=True)
        st = os.stat(p)
        files.append({'path': r, 'size': st.st_size, 'mtime_ns': st.st_mtime_ns, 'sha256': sha(p),
                      'compressed_sha256': sha(dst), 'compressed_size': os.path.getsize(dst)})
for d in dirs: os.makedirs(os.path.join(tree, d), exist_ok=True)
inner = os.path.join(work, part + '.tar')
with tarfile.open(inner, 'w') as tf: tf.add(tree, arcname=name, recursive=True)
open(inner + '.sha256', 'w').write(sha(inner) + '  ' + part + '.tar\n')
json.dump({'name': part, 'source_name': name, 'files': files, 'directories': dirs}, open(os.path.join(work, 'MANIFEST.json'), 'w'))
open(os.path.join(work, 'RESTORE.txt'), 'w').write('restore notes\n')
bundle = os.path.join(out, part + '.bundle.tar')
with tarfile.open(bundle, 'w') as tf:
    for p in [inner, inner + '.sha256', os.path.join(work, 'MANIFEST.json'), os.path.join(work, 'RESTORE.txt')]:
        tf.add(p, arcname=os.path.basename(p))
open(bundle + '.sha256', 'w').write(sha(bundle) + '  ' + part + '.bundle.tar\n')
"#;

fn tools() -> bool {
    let ok = |c: &str| Command::new(c).arg("--version").output().is_ok_and(|o| o.status.success());
    let have = ok("python3") && ok("zstd");
    if !have {
        eprintln!("skipping: python3 or zstd not installed");
    }
    have
}

fn bundle(src: &Path, out: &Path, part: &str, name: &str) -> PathBuf {
    let st = Command::new("python3").arg("-c").arg(BUILD).arg(src).arg(out).arg(part).arg(name).output().unwrap();
    assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
    out.join(format!("{part}.bundle.tar"))
}

fn setup(tmp: &Path) -> (Store, ImportOptions) {
    let store = Store::init(&tmp.join("arc"), Config { pack_target_bytes: 1 << 20, ..Config::default() }).unwrap();
    let opts = ImportOptions {
        dest: VPath::parse("/Imported").unwrap(),
        staging: tmp.join("staging"),
        receipts: tmp.join("arc/imports"),
        send: Default::default(),
    };
    (store, opts)
}

#[test]
fn parts_of_a_folder_become_one_project() {
    if !tools() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (src, out) = (tmp.path().join("src"), tmp.path().join("backups"));
    fs::create_dir_all(&out).unwrap();
    // One folder split into two parts, as move2storage did, plus another folder.
    make_tree(&src.join("Dairy"));
    let (a, b) = (src.join("part1"), src.join("part2"));
    for (dir, pick) in [(&a, true), (&b, false)] {
        for e in walkdir(&src.join("Dairy")) {
            if (e.to_string_lossy().contains("sub")) == pick {
                let rel = e.strip_prefix(src.join("Dairy")).unwrap();
                if e.is_file() {
                    fs::create_dir_all(dir.join(rel).parent().unwrap()).unwrap();
                    fs::copy(&e, dir.join(rel)).unwrap();
                    // fs::copy keeps dates on a Mac but not on Linux.
                    let mtime = filetime::FileTime::from_last_modification_time(&fs::metadata(&e).unwrap());
                    filetime::set_file_mtime(dir.join(rel), mtime).unwrap();
                }
            }
        }
    }
    write(&src.join("Notes/readme.txt"), b"notes");
    let bundles = vec![
        bundle(&b, &out, "Dairy.part0002", "Dairy"),
        bundle(&a, &out, "Dairy.part0001", "Dairy"),
        bundle(&src.join("Notes"), &out, "Notes.part0001", "Notes"),
    ];
    let before = fs::read_dir(&out).unwrap().count();

    let (mut store, opts) = setup(tmp.path());
    let receipts = import::import(&mut store, &bundles, &opts, &mut |_| {}).unwrap();
    assert_eq!(receipts.iter().map(|r| r.part.as_str()).collect::<Vec<_>>(), ["Dairy.part0001", "Dairy.part0002", "Notes.part0001"]);
    assert!(receipts.iter().all(|r| r.checked == "bundle"));

    // Both parts landed in one project, identical to the original folder.
    let dairy = store.stat(&VPath::parse("/Imported/Dairy").unwrap()).unwrap().unwrap();
    assert!(dairy.is_project);
    assert!(store.stat(&VPath::parse("/Imported/Notes").unwrap()).unwrap().unwrap().is_project);
    let got = tmp.path().join("got");
    let r = retrieve::retrieve(&mut store, &VPath::parse("/Imported/Dairy").unwrap(), &got, &RetrieveOptions::default(), &mut |_| {}).unwrap();
    assert!(r.ok(), "{r:?}");
    let mut want = snapshot(&src.join("Dairy"));
    want.retain(|_, n| !matches!(n, Node::Link(_)) && !matches!(n, Node::Dir));
    let mut have = snapshot(&got.join("Dairy"));
    have.retain(|_, n| !matches!(n, Node::Dir));
    assert_same(&have, &want);

    // Staging is emptied, bundles are untouched, and a second run skips everything.
    assert!(!opts.staging.join("Dairy.part0001").exists());
    assert_eq!(fs::read_dir(&out).unwrap().count(), before);
    let mut skipped = 0;
    import::import(&mut store, &bundles, &opts, &mut |e| skipped += matches!(e, import::Event::AlreadyDone { .. }) as usize).unwrap();
    assert_eq!(skipped, 3);
}

#[test]
fn damaged_bundles_are_refused() {
    if !tools() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("backups");
    fs::create_dir_all(&out).unwrap();
    write(&tmp.path().join("src/Run/data.bin"), &text(300_000, 3));
    let b = bundle(&tmp.path().join("src/Run"), &out, "Run.part0001", "Run");
    let (mut store, opts) = setup(tmp.path());

    // A changed byte anywhere: the bundle's checksum catches it.
    let mut bytes = fs::read(&b).unwrap();
    let good = bytes.clone();
    let k = bytes.len() / 2;
    bytes[k] ^= 1;
    fs::write(&b, &bytes).unwrap();
    let e = import::import(&mut store, &[b.clone()], &opts, &mut |_| {}).unwrap_err();
    assert!(e.to_string().contains("doesn't match its .sha256"), "{e}");

    // Without that file, each file is still checked against the manifest.
    fs::write(&b, &good).unwrap();
    fs::remove_file(out.join("Run.part0001.bundle.tar.sha256")).unwrap();
    let key = b"\"sha256\": \"";
    let manifest = good.windows(key.len()).position(|w| w == key).unwrap() + key.len();
    let mut bad = good.clone();
    bad[manifest] = if bad[manifest] == b'0' { b'1' } else { b'0' };
    fs::write(&b, &bad).unwrap();
    let e = import::import(&mut store, &[b.clone()], &opts, &mut |_| {}).unwrap_err();
    assert!(e.to_string().contains("doesn't match its recorded checksum"), "{e}");
    assert!(store.stat(&VPath::parse("/Imported/Run").unwrap()).unwrap().is_none());

    // Intact, it imports.
    fs::write(&b, &good).unwrap();
    let r = import::import(&mut store, &[b], &opts, &mut |_| {}).unwrap();
    assert_eq!(r[0].checked, "inner tar");
}

fn walkdir(p: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(p).unwrap() {
        let path = e.unwrap().path();
        let md = fs::symlink_metadata(&path).unwrap();
        if md.is_dir() {
            out.extend(walkdir(&path));
        } else if md.is_file() {
            out.push(path);
        }
    }
    out
}

/// Python that stages a folder the way move2storage's server did before
/// bundling: `<part>/tree/<name>/<rel>.zst` plus one receipt per file.
const STAGE: &str = r#"
import hashlib, json, os, subprocess, sys
src, job, part, name = sys.argv[1:5]
def sha(p):
    h = hashlib.sha256()
    with open(p, 'rb') as f:
        for b in iter(lambda: f.read(1 << 20), b''): h.update(b)
    return h.hexdigest()
allowed_path = os.path.join(job, 'allowed-parts.json')
allowed = json.load(open(allowed_path)) if os.path.exists(allowed_path) else {}
allowed[part] = name
os.makedirs(job, exist_ok=True); json.dump(allowed, open(allowed_path, 'w'))
work = os.path.join(job, part); tree = os.path.join(work, 'tree', name); rec = os.path.join(work, 'receipts')
os.makedirs(rec, exist_ok=True)
for d, ds, fs in os.walk(src):
    for x in fs:
        p = os.path.join(d, x); r = os.path.normpath(os.path.relpath(p, src)); dst = os.path.join(tree, r + '.zst')
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        subprocess.run(['zstd', '-q', '-3', '-f', p, '-o', dst], check=True)
        st = os.stat(p)
        r = {'path': r, 'size': st.st_size, 'mtime_ns': st.st_mtime_ns, 'dev': 1, 'ino': 2, 'name': part, 'sha256': sha(p),
             'compressed_sha256': sha(dst), 'compressed_size': os.path.getsize(dst)}
        json.dump(r, open(os.path.join(rec, hashlib.sha256(r['path'].encode()).hexdigest() + '.json'), 'w'))
"#;

fn stage(src: &Path, job: &Path, part: &str, name: &str) {
    let st = Command::new("python3").arg("-c").arg(STAGE).arg(src).arg(job).arg(part).arg(name).output().unwrap();
    assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
}

#[test]
fn staged_parts_import_and_bundled_ones_are_skipped() {
    if !tools() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let job = tmp.path().join("backups/.move2storage-job");
    let src = tmp.path().join("src");
    write(&src.join("a/reads_1.fastq"), &text(400_000, 1));
    write(&src.join("a/sub/notes.txt"), b"part one");
    write(&src.join("b/reads_2.fastq"), &text(500_000, 2));
    stage(&src.join("a"), &job, "Run.part0001", "Run");
    stage(&src.join("b"), &job, "Run.part0002", "Run");
    // A file still arriving when the tool stopped: no receipt, left out.
    write(&job.join("Run.part0002/tree/Run/half.bin.zst.partial"), b"unfinished");
    // A part that was bundled has only complete.json left.
    let mut allowed: serde_json::Value = serde_json::from_slice(&fs::read(job.join("allowed-parts.json")).unwrap()).unwrap();
    allowed["Done.part0001"] = "Done".into();
    fs::write(job.join("allowed-parts.json"), allowed.to_string()).unwrap();
    write(&job.join("Done.part0001/complete.json"), b"{}");

    let (mut store, opts) = setup(tmp.path());
    let mut skipped = Vec::new();
    let r = import::import_staged(&mut store, &job, &opts, &mut |e| {
        if let import::Event::Skipped { part, reason } = e {
            skipped.push(format!("{part}: {reason}"));
        }
    })
    .unwrap();
    assert_eq!(r.len(), 2);
    assert!(r.iter().all(|x| x.checked == "server receipts" && x.project == "/Imported/Run"));
    assert_eq!(skipped.len(), 2, "{skipped:?}");
    assert!(skipped.iter().any(|s| s.starts_with("Done.part0001") && s.contains("bundled")));
    assert!(skipped.iter().any(|s| s.contains("1 unfinished")));
    let run = store.stat(&VPath::parse("/Imported/Run").unwrap()).unwrap().unwrap();
    assert!(run.is_project);
    assert_eq!(run.files, 3);
    let mut got = String::new();
    std::io::Read::read_to_string(&mut store.read_file(&VPath::parse("/Imported/Run/sub/notes.txt").unwrap()).unwrap().1, &mut got).unwrap();
    assert_eq!(got, "part one");
    // The job folder is left exactly as it was.
    assert!(job.join("Run.part0001/tree/Run/reads_1.fastq.zst").exists());

    // A staged file that doesn't match its receipt stops the import.
    stage(&src.join("a"), &job, "Bad.part0001", "Bad");
    fs::write(job.join("Bad.part0001/tree/Bad/sub/notes.txt.zst"), zstd::encode_all(&b"tampered"[..], 3).unwrap()).unwrap();
    let e = import::import_staged(&mut store, &job, &opts, &mut |_| {}).unwrap_err();
    assert!(e.to_string().contains("doesn't match its recorded checksum"), "{e}");
    assert!(store.stat(&VPath::parse("/Imported/Bad").unwrap()).unwrap().is_none());
}
