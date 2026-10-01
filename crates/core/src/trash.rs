//! The Trash on a file server. A plain file server has none, so the helper keeps one: sending an
//! item to the Trash *moves* it (instantly, however big it is) into a hidden folder, with a note of
//! where it came from, and it stays until someone restores it or deletes it for good. Nothing is
//! deleted automatically.
//!
//! Each item gets an id and a note `items/<id>.json` under the trash folder in the user's home. It
//! is stored at `items/<id>/<name>` there. A move can't cross from one disk to another, so an item
//! on a different disk than the home folder stays on its own disk, in a hidden
//! `.qm-trash/<id>/<name>` beside where it was; its note says where. If even that can't be made
//! (a folder the user can't write to), the item isn't trashed and is reported, so the person can
//! choose to delete it for good instead.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::util;

/// A folder left beside the original when the home folder is on another disk.
const BESIDE: &str = ".qm-trash";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trashed {
    pub id: String,
    pub name: String,
    /// Where it was, to put it back.
    pub original: String,
    /// Where it is now.
    pub stored: String,
    /// "file" or "dir".
    pub kind: String,
    /// Size of a file (0 for a folder; count a folder with `FsMeasure` on `stored`).
    pub size: u64,
    /// Seconds since the epoch.
    pub trashed_at: i64,
}

/// An item that couldn't be sent to the Trash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub path: String,
    pub reason: String,
    /// The Trash couldn't be made for it, but deleting it for good would be allowed.
    pub can_delete: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub trashed: Vec<Trashed>,
    pub failed: Vec<Failure>,
}

/// `~/.local/share/archive-helper/trash`.
pub fn default_dir() -> PathBuf {
    crate::fsview::home().join(".local/share/archive-helper/trash")
}

fn notes(base: &Path) -> PathBuf {
    base.join("items")
}

fn write_note(base: &Path, t: &Trashed) -> Result<()> {
    fs::create_dir_all(notes(base))?;
    util::atomic_write(&notes(base).join(format!("{}.json", t.id)), &serde_json::to_vec_pretty(t)?)?;
    Ok(())
}

/// Why this can't be sent to the Trash at all (the same things that may never be deleted, plus
/// what is already in the Trash).
fn refused(path: &Path, base: &Path, home: &Path) -> Option<String> {
    if let Some(why) = crate::fsview::protected(path, home) {
        return Some(why);
    }
    if path.starts_with(base) || base.starts_with(path) || path.components().any(|c| c.as_os_str() == BESIDE) {
        return Some("that is already in the Trash".into());
    }
    None
}

fn crosses_disks(e: &std::io::Error) -> bool {
    // EXDEV: "Invalid cross-device link".
    e.raw_os_error() == Some(18)
}

/// Send these items to the Trash. If any is refused (the top of a disk, the home folder, something
/// already in the Trash) or missing, nothing is moved and the request fails. Otherwise each item
/// is moved, and one that couldn't be (no permission, say) is reported in `failed` while the rest
/// still go.
pub fn send_in(base: &Path, home: &Path, paths: &[PathBuf]) -> Result<Report> {
    for p in paths {
        if let Some(why) = refused(p, base, home) {
            return Err(Error::InvalidPath(format!("Not sent to the Trash: {} — {why}.", p.display())));
        }
        if fs::symlink_metadata(p).is_err() {
            return Err(Error::NotFound(format!("{} isn't there any more.", p.display())));
        }
    }
    let mut report = Report::default();
    for p in paths {
        match send_one(base, p) {
            Ok(t) => report.trashed.push(t),
            Err(reason) => report.failed.push(Failure { path: p.display().to_string(), reason, can_delete: true }),
        }
    }
    Ok(report)
}

fn send_one(base: &Path, p: &Path) -> std::result::Result<Trashed, String> {
    let md = fs::symlink_metadata(p).map_err(|e| e.to_string())?;
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let id = util::random_hex(6);
    let home_side = notes(base).join(&id);
    let stored = match fs::create_dir_all(&home_side).and_then(|_| fs::rename(p, home_side.join(&name))) {
        Ok(()) => home_side.join(&name),
        Err(e) => {
            let _ = fs::remove_dir(&home_side);
            if !crosses_disks(&e) {
                return Err(format!("the Trash couldn't be made ({e})"));
            }
            // Another disk: keep it there, out of sight, beside where it was.
            let beside = p.parent().unwrap_or(Path::new("/")).join(BESIDE).join(&id);
            let moved = fs::create_dir_all(&beside).and_then(|_| fs::rename(p, beside.join(&name)));
            if let Err(e) = moved {
                let _ = fs::remove_dir(&beside);
                return Err(format!("a Trash can't be made on that disk ({e})"));
            }
            beside.join(&name)
        }
    };
    let t = Trashed {
        id,
        name,
        original: p.display().to_string(),
        stored: stored.display().to_string(),
        kind: if md.is_dir() { "dir" } else { "file" }.into(),
        size: if md.is_dir() { 0 } else { md.len() },
        trashed_at: util::now_secs(),
    };
    // If the note can't be written the item is still safe where it is, but nothing could find it.
    if let Err(e) = write_note(base, &t) {
        let _ = fs::rename(&stored, p);
        return Err(format!("the Trash couldn't keep a note of it ({e})"));
    }
    Ok(t)
}

/// Forget an item's note and the (now empty) folder that held it.
fn forget(base: &Path, t: &Trashed) {
    let _ = fs::remove_file(notes(base).join(format!("{}.json", t.id)));
    if let Some(holder) = Path::new(&t.stored).parent() {
        let _ = fs::remove_dir(holder);
        if let Some(beside) = holder.parent().filter(|p| p.file_name().is_some_and(|n| n == BESIDE)) {
            let _ = fs::remove_dir(beside);
        }
    }
}

/// What is in the Trash, newest first. An item whose stored copy is gone (removed by hand) is
/// dropped from the list.
pub fn list_in(base: &Path) -> Vec<Trashed> {
    let mut out: Vec<Trashed> = Vec::new();
    for e in fs::read_dir(notes(base)).into_iter().flatten().flatten() {
        if e.path().extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Some(t) = fs::read(e.path()).ok().and_then(|b| serde_json::from_slice::<Trashed>(&b).ok()) else { continue };
        if fs::symlink_metadata(&t.stored).is_ok() {
            out.push(t);
        } else {
            forget(base, &t);
        }
    }
    out.sort_by_key(|t| std::cmp::Reverse(t.trashed_at));
    out
}

/// Put an item back where it was (or into the folder `to`). Nothing is overwritten.
pub fn restore_in(base: &Path, id: &str, to: Option<&Path>) -> Result<String> {
    let note_path = notes(base).join(format!("{id}.json"));
    let t: Trashed = serde_json::from_slice(&fs::read(&note_path).map_err(|_| Error::NotFound("that item isn't in the Trash any more".into()))?)?;
    let target = match to {
        Some(dir) => dir.join(&t.name),
        None => PathBuf::from(&t.original),
    };
    if fs::symlink_metadata(&target).is_ok() {
        return Err(Error::AlreadyExists(target.display().to_string()));
    }
    match target.parent() {
        Some(parent) if parent.is_dir() => {}
        _ => {
            let from = target.parent().map(|p| p.display().to_string()).unwrap_or_default();
            return Err(Error::NotFound(format!("the folder it came from ({from}) is gone")));
        }
    }
    fs::rename(&t.stored, &target).map_err(|e| Error::other(format!("Couldn't put it back: {e}")))?;
    forget(base, &t);
    Ok(target.display().to_string())
}

/// Delete items in the Trash for good: the ones with these ids, or all of them. Returns how many.
pub fn empty_in(base: &Path, home: &Path, ids: Option<&[String]>) -> Result<usize> {
    let mut count = 0;
    for t in list_in(base) {
        if ids.is_some_and(|ids| !ids.contains(&t.id)) {
            continue;
        }
        crate::fsview::delete_paths_in(home, &[PathBuf::from(&t.stored)]).map_err(Error::other)?;
        forget(base, &t);
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let base = home.join(".local/share/archive-helper/trash");
        let data = home.join("data");
        fs::create_dir_all(&data).unwrap();
        (tmp, home, base, data)
    }

    #[test]
    fn a_trashed_item_can_be_brought_back() {
        let (_t, home, base, data) = setup();
        fs::create_dir_all(data.join("run42/sub")).unwrap();
        fs::write(data.join("run42/sub/a.txt"), b"hello").unwrap();
        fs::write(data.join("notes.txt"), b"n").unwrap();
        let report = send_in(&base, &home, &[data.join("run42"), data.join("notes.txt")]).unwrap();
        assert!(report.failed.is_empty());
        assert_eq!(report.trashed.len(), 2);
        assert!(!data.join("run42").exists() && !data.join("notes.txt").exists(), "they were moved, not copied");
        assert_eq!((report.trashed[0].kind.as_str(), report.trashed[1].size), ("dir", 1));
        assert_eq!(list_in(&base).len(), 2);

        let back = restore_in(&base, &report.trashed[0].id, None).unwrap();
        assert_eq!(back, data.join("run42").display().to_string());
        assert_eq!(fs::read(data.join("run42/sub/a.txt")).unwrap(), b"hello");
        assert_eq!(list_in(&base).len(), 1, "only the other one is left");
    }

    #[test]
    fn restoring_never_overwrites() {
        let (_t, home, base, data) = setup();
        fs::write(data.join("a.txt"), b"old").unwrap();
        let t = send_in(&base, &home, &[data.join("a.txt")]).unwrap().trashed.remove(0);
        fs::write(data.join("a.txt"), b"a new one made since").unwrap();
        assert!(matches!(restore_in(&base, &t.id, None), Err(Error::AlreadyExists(_))));
        assert_eq!(fs::read(data.join("a.txt")).unwrap(), b"a new one made since");
        let other = data.join("elsewhere");
        fs::create_dir_all(&other).unwrap();
        restore_in(&base, &t.id, Some(&other)).unwrap();
        assert_eq!(fs::read(other.join("a.txt")).unwrap(), b"old");
    }

    #[test]
    fn the_dangerous_and_the_missing_are_refused_and_nothing_is_moved() {
        let (_t, home, base, data) = setup();
        fs::write(data.join("keep.txt"), b"k").unwrap();
        for bad in [PathBuf::from("/"), home.clone(), home.parent().unwrap().to_path_buf(), base.clone()] {
            assert!(send_in(&base, &home, &[data.join("keep.txt"), bad.clone()]).is_err(), "{}", bad.display());
        }
        assert!(data.join("keep.txt").exists(), "a refused request moves nothing at all");
        assert!(matches!(send_in(&base, &home, &[data.join("missing")]), Err(Error::NotFound(_))));
        // Something already in the Trash isn't trashed again.
        let t = send_in(&base, &home, &[data.join("keep.txt")]).unwrap().trashed.remove(0);
        assert!(send_in(&base, &home, &[PathBuf::from(&t.stored)]).is_err());
    }

    #[test]
    fn emptying_deletes_for_good_the_chosen_items_or_all() {
        let (_t, home, base, data) = setup();
        for n in ["a", "b", "c"] {
            fs::create_dir_all(data.join(n)).unwrap();
            fs::write(data.join(n).join("f.txt"), n.as_bytes()).unwrap();
        }
        let ids: Vec<Trashed> = send_in(&base, &home, &[data.join("a"), data.join("b"), data.join("c")]).unwrap().trashed;
        let stored: Vec<String> = ids.iter().map(|t| t.stored.clone()).collect();
        assert_eq!(empty_in(&base, &home, Some(&[ids[0].id.clone()])).unwrap(), 1);
        assert!(!Path::new(&stored[0]).exists(), "gone for good");
        assert!(Path::new(&stored[1]).exists() && Path::new(&stored[2]).exists(), "the others stay");
        assert_eq!(list_in(&base).len(), 2);
        assert_eq!(empty_in(&base, &home, None).unwrap(), 2);
        assert!(list_in(&base).is_empty());
        assert!(!Path::new(&stored[1]).exists());
    }

    #[test]
    fn something_removed_by_hand_drops_out_of_the_list() {
        let (_t, home, base, data) = setup();
        fs::write(data.join("a.txt"), b"a").unwrap();
        let t = send_in(&base, &home, &[data.join("a.txt")]).unwrap().trashed.remove(0);
        fs::remove_file(&t.stored).unwrap();
        assert!(list_in(&base).is_empty());
        assert!(matches!(restore_in(&base, &t.id, None), Err(Error::NotFound(_))));
    }

    #[test]
    fn an_item_that_cant_be_moved_is_reported_and_the_others_still_go() {
        let (_t, home, base, data) = setup();
        fs::write(data.join("ok.txt"), b"1").unwrap();
        // A trash that can't be made: its folder's place is taken by a file.
        fs::create_dir_all(base.parent().unwrap()).unwrap();
        fs::write(&base, b"in the way").unwrap();
        let report = send_in(&base, &home, &[data.join("ok.txt")]).unwrap();
        assert!(report.trashed.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert!(report.failed[0].can_delete && report.failed[0].reason.contains("Trash"), "{:?}", report.failed);
        assert!(data.join("ok.txt").exists(), "nothing was lost");
    }
}
