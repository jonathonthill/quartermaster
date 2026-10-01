//! Helpers shared by the integration tests.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use archive_core::hash::Digest;
use unicode_normalization::UnicodeNormalization;

pub fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

pub fn text(n: usize, seed: u64) -> Vec<u8> {
    let words = ["ACGT", "sample", "0.125", "chr22", "\t", "\n", "read_id", "GATTACA"];
    let mut x = seed | 1;
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        out.extend_from_slice(words[(x >> 33) as usize % words.len()].as_bytes());
    }
    out.truncate(n);
    out
}

pub fn write(p: &Path, data: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, data).unwrap();
}

/// A tree exercising the awkward cases.
pub fn make_tree(root: &Path) {
    write(&root.join("empty.txt"), b"");
    write(&root.join("small.csv"), &text(5_000, 1));
    write(&root.join("sub/deeper/notes.md"), &text(900, 2));
    write(&root.join("sub/big_text.fastq"), &text(3 << 20, 3));
    write(&root.join("sub/random.bin"), &noise(2 << 20, 4));
    write(&root.join("Données über/résumé (final).txt"), &text(700, 5));
    write(&root.join("spaces in name/a b c.txt"), b"abc");
    write(&root.join(format!("{}/long.txt", "l".repeat(120))), b"long path");
    // A large file "x" is stored as "x.zst"; a small original "x.zst" must not collide.
    write(&root.join("collide/x"), &text(1_500_000, 6));
    write(&root.join("collide/x.zst"), b"not really zstd");
    // Both large: stored as members "y.zst" and "y.zst.zst".
    write(&root.join("collide/y"), &text(1_500_000, 8));
    write(&root.join("collide/y.zst"), &noise(1_500_000, 9));
    write(&root.join("dup_a.dat"), &text(200_000, 7));
    write(&root.join("dup_b.dat"), &text(200_000, 7));
    // Identical twins with different dates: a recovered copy must keep its own.
    filetime::set_file_mtime(root.join("dup_a.dat"), filetime::FileTime::from_unix_time(1_600_000_000, 0)).unwrap();
    filetime::set_file_mtime(root.join("dup_b.dat"), filetime::FileTime::from_unix_time(1_700_000_123, 0)).unwrap();
    fs::create_dir_all(root.join("empty_dir")).unwrap();
    write(&root.join(".DS_Store"), b"junk");
    write(&root.join("sub/._random.bin"), b"appledouble");
    #[cfg(unix)]
    std::os::unix::fs::symlink("sub/notes.md", root.join("link_to_notes")).unwrap();
}

#[derive(PartialEq)]
pub enum Node {
    Dir,
    File(Vec<u8>, i64),
    Link(String),
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Node::Dir => write!(f, "Dir"),
            Node::File(d, t) => write!(f, "File({} bytes, sha {}, mtime {t})", d.len(), &Digest::of(d).to_hex()[..10]),
            Node::Link(t) => write!(f, "Link({t})"),
        }
    }
}

/// Assert two snapshots match, printing only the differences.
pub fn assert_same(got: &BTreeMap<String, Node>, want: &BTreeMap<String, Node>) {
    let mut diffs = Vec::new();
    for (k, v) in want {
        match got.get(k) {
            None => diffs.push(format!("missing: {k} ({v:?})")),
            Some(g) if g != v => diffs.push(format!("differs: {k}: got {g:?}, want {v:?}")),
            _ => {}
        }
    }
    for k in got.keys().filter(|k| !want.contains_key(*k)) {
        diffs.push(format!("extra: {k} ({:?})", got[k]));
    }
    assert!(diffs.is_empty(), "snapshot differences:\n{}", diffs.join("\n"));
}

pub fn snapshot(root: &Path) -> BTreeMap<String, Node> {
    let mut out = BTreeMap::new();
    fn go(base: &Path, p: &Path, out: &mut BTreeMap<String, Node>) {
        for e in fs::read_dir(p).unwrap() {
            let e = e.unwrap();
            // macOS tar writes names in decomposed Unicode (NFD); compare in NFC.
            let name: String = e.file_name().to_str().unwrap().nfc().collect();
            if name == ".DS_Store" || name.starts_with("._") || name.ends_with(".archive-partial") {
                continue;
            }
            let path = e.path();
            let rel: String = path.strip_prefix(base).unwrap().to_str().unwrap().nfc().collect();
            let md = fs::symlink_metadata(&path).unwrap();
            if md.file_type().is_symlink() {
                out.insert(rel, Node::Link(fs::read_link(&path).unwrap().to_str().unwrap().into()));
            } else if md.is_dir() {
                out.insert(rel, Node::Dir);
                go(base, &path, out);
            } else {
                let secs = md.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
                out.insert(rel, Node::File(fs::read(&path).unwrap(), secs));
            }
        }
    }
    go(root, root, &mut out);
    out
}
