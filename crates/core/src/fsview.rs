//! Browsing an ordinary file system: this computer in the app, or a file
//! server through `archive-helper files`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::send::{SYSTEM_JUNK, glob_match};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Crumb {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
    pub name: String,
    pub path: String,
    /// "dir", "file", or "link".
    pub kind: String,
    pub size: Option<u64>,
    /// Files inside (archive folders only).
    pub files: Option<u64>,
    /// Modified time, seconds since the epoch.
    pub mtime: i64,
    /// When it was archived (archive items only).
    pub archived: Option<i64>,
    /// A project's top folder (archive items only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_project: bool,
    /// Inside a project, so it can't be changed (archive items only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_project: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Listing {
    pub path: String,
    pub parent: Option<String>,
    pub crumbs: Vec<Crumb>,
    pub items: Vec<Item>,
    pub free_bytes: Option<u64>,
    /// Archives: the project this folder is (or is inside), by its path.
    /// Sending here adds to that project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Place {
    pub name: String,
    pub path: String,
}

fn is_junk(name: &str) -> bool {
    SYSTEM_JUNK.iter().any(|p| glob_match(p, name))
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn item_for(path: &Path, name: String, md: &fs::Metadata) -> Item {
    let kind = if md.file_type().is_symlink() {
        // Follow links to folders so they can be opened.
        if path.is_dir() { "dir" } else { "link" }
    } else if md.is_dir() {
        "dir"
    } else {
        "file"
    };
    let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
    Item {
        name,
        path: path.display().to_string(),
        kind: kind.to_string(),
        size: (kind == "file").then_some(md.len()),
        files: None,
        mtime,
        archived: None,
        is_project: false,
        in_project: false,
    }
}

/// A folder path as a person might type it: a leading `~` means the home
/// folder, and `.` and `..` are resolved (without following links).
pub fn typed_path(path: &str) -> Result<PathBuf, String> {
    let path = path.trim();
    let p = if path == "~" {
        home()
    } else if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        home().join(rest)
    } else {
        PathBuf::from(path)
    };
    if !p.is_absolute() {
        return Err(format!("Type a full path, starting with / or ~ (not “{path}”)."));
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

pub fn list(path: Option<&str>, show_hidden: bool) -> Result<Listing, String> {
    let dir = match path {
        Some(p) => typed_path(p)?,
        None => home(),
    };
    let rd = fs::read_dir(&dir).map_err(|e| format!("Can't open {}: {e}", dir.display()))?;
    let mut items = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if is_junk(&name) || (!show_hidden && name.starts_with('.')) {
            continue;
        }
        let Ok(md) = fs::symlink_metadata(e.path()) else { continue };
        items.push(item_for(&e.path(), name, &md));
    }
    sort(&mut items);
    Ok(Listing {
        path: dir.display().to_string(),
        parent: dir.parent().map(|p| p.display().to_string()),
        crumbs: crumbs(&dir),
        items,
        free_bytes: crate::util::free_space(&dir),
        project: None,
    })
}

pub fn sort(items: &mut [Item]) {
    items.sort_by(|a, b| (a.kind != "dir").cmp(&(b.kind != "dir")).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
}

fn crumbs(dir: &Path) -> Vec<Crumb> {
    let mut out = Vec::new();
    let mut cur = PathBuf::new();
    for c in dir.components() {
        cur.push(c.as_os_str());
        let name = match c {
            std::path::Component::RootDir => "/".to_string(),
            other => other.as_os_str().to_string_lossy().into_owned(),
        };
        out.push(Crumb { name, path: cur.display().to_string() });
    }
    // Show paths under the home folder starting from it.
    let h = home();
    if let Some(i) = out.iter().position(|c| Path::new(&c.path) == h) {
        out.drain(..i);
        if let Some(first) = out.first_mut() {
            first.name = "Home".into();
        }
    }
    out
}

/// A cloud storage folder's name as a person would say it. The Mac keeps each
/// provider's folder in `~/Library/CloudStorage` as `Provider-Account`:
/// "Box-Box" is Box, "GoogleDrive-me@gmail.com" is "Google Drive (me@gmail.com)".
fn cloud_name(folder: &str) -> String {
    let (provider, account) = folder.split_once('-').unwrap_or((folder, ""));
    let provider = match provider {
        "GoogleDrive" => "Google Drive",
        "OneDrive" => "OneDrive",
        "Dropbox" => "Dropbox",
        "Box" => "Box",
        other => other,
    };
    // Box names its account "Box" too; say nothing more than the provider then.
    if account.is_empty() || account == provider || account == "Box" { provider.to_string() } else { format!("{provider} ({account})") }
}

/// Folders synced by Box, Dropbox, Google Drive, OneDrive, and iCloud Drive.
fn cloud_places(home: &Path) -> Vec<Place> {
    let mut found: Vec<Place> = Vec::new();
    let mut add = |name: String, path: PathBuf| {
        if path.is_dir() && !found.iter().any(|p| p.path == path.display().to_string()) {
            found.push(Place { name, path: path.display().to_string() });
        }
    };
    if cfg!(target_os = "macos") {
        // The providers' own folders, whatever accounts are signed in.
        if let Ok(rd) = fs::read_dir(home.join("Library/CloudStorage")) {
            let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !n.starts_with('.')).collect();
            names.sort_by_key(|n| n.to_lowercase());
            for n in names {
                add(cloud_name(&n), home.join("Library/CloudStorage").join(&n));
            }
        }
        add("iCloud Drive".into(), home.join("Library/Mobile Documents/com~apple~CloudDocs"));
    } else {
        // Elsewhere the sync apps make their folder in the home folder.
        if let Ok(rd) = fs::read_dir(home) {
            let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            names.sort_by_key(|n| n.to_lowercase());
            for n in names {
                let label = match n.as_str() {
                    "Box" | "Dropbox" | "Google Drive" | "OneDrive" => Some(n.clone()),
                    _ if n.starts_with("OneDrive - ") => Some(format!("OneDrive ({})", &n["OneDrive - ".len()..])),
                    _ => None,
                };
                if let Some(label) = label {
                    add(label, home.join(&n));
                }
            }
        }
    }
    found
}

pub fn places() -> Vec<Place> {
    let h = home();
    let mut out = vec![Place { name: "Home".into(), path: h.display().to_string() }];
    for (name, sub) in [("Desktop", "Desktop"), ("Documents", "Documents"), ("Downloads", "Downloads")] {
        let p = h.join(sub);
        if p.is_dir() {
            out.push(Place { name: name.into(), path: p.display().to_string() });
        }
    }
    out.extend(cloud_places(&h));
    let user = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_default();
    let mut volumes = |dir: &str| {
        let Ok(rd) = fs::read_dir(dir) else { return };
        let mut found: Vec<Place> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                // /media/<user> holds that user's own drives, which are listed separately.
                let skip = name.starts_with('.') || (dir == "/media" && name == user) || !e.path().is_dir();
                (!skip).then(|| Place { name, path: e.path().display().to_string() })
            })
            .collect();
        found.sort_by_key(|p| p.name.to_lowercase());
        out.extend(found);
    };
    if cfg!(target_os = "macos") {
        volumes("/Volumes");
    } else if cfg!(unix) {
        // Linux and FreeBSD: drives mounted per user (/media/<user>/USB,
        // /run/media/<user>/USB), directly in /media (/media/storage), or in /mnt.
        if !user.is_empty() {
            volumes(&format!("/media/{user}"));
            volumes(&format!("/run/media/{user}"));
        }
        volumes("/media");
        volumes("/mnt");
    } else if cfg!(windows) {
        for d in b'C'..=b'Z' {
            let p = format!("{}:\\", d as char);
            if Path::new(&p).is_dir() {
                out.push(Place { name: format!("Drive {}", d as char), path: p });
            }
        }
    }
    out
}

// ---------------------------------------------------------------- deleting

/// Why `path` must never be deleted, whatever was asked: the top level of a disk, the home
/// folder and anything holding it, and (on Unix) a separate disk mounted there.
pub fn protected(path: &Path, home: &Path) -> Option<String> {
    if path.parent().is_none() || path.parent() == Some(Path::new("/")) {
        return Some("that is the top level of a disk, not something to delete".into());
    }
    if home.starts_with(path) {
        return Some("that holds your home folder".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = |p: &Path| fs::symlink_metadata(p).ok().map(|m| m.dev());
        if let (Some(a), Some(b)) = (dev(path), path.parent().and_then(dev)) {
            if a != b {
                return Some("that is a separate disk (a mount point), not a folder on this one".into());
            }
        }
    }
    None
}

/// A folder inside `dir` that is on another disk (mounted there), if any. Deleting `dir` would
/// reach into it.
#[cfg(unix)]
fn foreign_disk_inside(dir: &Path, dev: u64) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    for e in fs::read_dir(dir).ok()?.flatten() {
        let Ok(md) = fs::symlink_metadata(e.path()) else { continue };
        if !md.is_dir() {
            continue;
        }
        if md.dev() != dev {
            return Some(e.path());
        }
        if let Some(found) = foreign_disk_inside(&e.path(), dev) {
            return Some(found);
        }
    }
    None
}

/// Delete a folder's contents and then the folder, never following a link and never leaving
/// the disk it started on.
fn delete_tree(path: &Path) -> std::io::Result<()> {
    for e in fs::read_dir(path)? {
        let p = e?.path();
        if fs::symlink_metadata(&p)?.is_dir() {
            delete_tree(&p)?;
        } else {
            fs::remove_file(&p)?;
        }
    }
    fs::remove_dir(path)
}

/// Delete these files and folders (everything inside a folder too), for good. Everything is
/// checked first (each must exist, none may be protected, and no folder may hold another disk),
/// so a refused request deletes nothing. Returns how many were deleted.
pub fn delete_paths_in(home: &Path, paths: &[PathBuf]) -> Result<usize, String> {
    for p in paths {
        if let Some(why) = protected(p, home) {
            return Err(format!("Not deleted: {} — {why}.", p.display()));
        }
        let md = fs::symlink_metadata(p).map_err(|_| format!("{} isn't there any more.", p.display()))?;
        #[cfg(unix)]
        if md.is_dir() {
            use std::os::unix::fs::MetadataExt;
            if let Some(inner) = foreign_disk_inside(p, md.dev()) {
                return Err(format!("Not deleted: {} holds another disk ({}), which would be deleted with it.", p.display(), inner.display()));
            }
        }
        let _ = md;
    }
    for (done, p) in paths.iter().enumerate() {
        let result = match fs::symlink_metadata(p) {
            Ok(md) if md.is_dir() => delete_tree(p),
            Ok(_) => fs::remove_file(p),
            Err(_) => Ok(()),
        };
        if let Err(e) = result {
            let deleted = if done == 0 { String::new() } else { format!(" ({done} of {} were deleted first)", paths.len()) };
            return Err(format!("Couldn't delete {}: {e}{deleted}", p.display()));
        }
    }
    Ok(paths.len())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hit {
    /// The folder the match is in (absolute, for opening it).
    pub folder: String,
    pub item: Item,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchResult {
    pub hits: Vec<Hit>,
    /// More matches exist than were returned.
    pub truncated: bool,
    /// The time limit ran out before every subfolder was searched.
    pub timed_out: bool,
    /// Files and folders looked at (file-by-file searches).
    pub scanned: u64,
    /// "spotlight", "locate", or "scan".
    pub method: crate::findfiles::Method,
}

/// Find names under `root`: through Spotlight (or locate) when the folder is
/// indexed, otherwise file by file. See [`crate::findfiles`].
pub fn search(root: &str, query: &str, show_hidden: bool, every_file: bool, cancelled: &dyn Fn() -> bool) -> Result<SearchResult, String> {
    use crate::findfiles::{FindOptions, find};
    let opts = FindOptions { show_hidden, every_file, ..FindOptions::default() };
    let r = find(Path::new(root), query, &opts, cancelled).map_err(|e| match e {
        crate::Error::InvalidPath(m) => m,
        other => other.to_string(),
    })?;
    let hits = r
        .found
        .into_iter()
        .map(|f| {
            let name = f.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let folder = f.path.parent().map(|p| p.display().to_string()).unwrap_or_default();
            Hit { folder, item: item_for(&f.path, name, &f.meta) }
        })
        .collect();
    Ok(SearchResult { hits, truncated: r.truncated, timed_out: r.timed_out, scanned: r.scanned, method: r.method })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Measure {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
}

/// Count files and bytes under `paths` (skipping OS junk files).
pub fn measure(paths: &[String]) -> Measure {
    let mut m = Measure::default();
    let mut stack: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    while let Some(p) = stack.pop() {
        let Ok(md) = fs::symlink_metadata(&p) else { continue };
        if md.is_dir() {
            m.folders += 1;
            if let Ok(rd) = fs::read_dir(&p) {
                for e in rd.flatten() {
                    if !is_junk(&e.file_name().to_string_lossy()) {
                        stack.push(e.path());
                    }
                }
            }
        } else if md.is_file() {
            m.files += 1;
            m.bytes += md.len();
        }
    }
    m
}

#[cfg(test)]
mod tests {
    #[test]
    fn deleting_removes_folders_with_everything_in_them() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let data = home.join("data");
        std::fs::create_dir_all(data.join("run42/sub/deep")).unwrap();
        std::fs::write(data.join("run42/sub/a.txt"), b"a").unwrap();
        std::fs::write(data.join("run42/b.txt"), b"b").unwrap();
        std::fs::write(data.join("keep.txt"), b"k").unwrap();
        assert_eq!(super::delete_paths_in(&home, &[data.join("run42")]).unwrap(), 1);
        assert!(!data.join("run42").exists());
        assert!(data.join("keep.txt").exists(), "only what was asked for");
    }

    #[cfg(unix)]
    #[test]
    fn deleting_a_link_removes_the_link_not_what_it_points_to() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let target = home.join("important");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("precious.txt"), b"p").unwrap();
        let link = home.join("data/link_to_important");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        super::delete_paths_in(&home, &[link.clone()]).unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err());
        assert_eq!(std::fs::read(target.join("precious.txt")).unwrap(), b"p");
    }

    #[test]
    fn the_dangerous_ones_are_refused_and_nothing_is_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let data = home.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("keep.txt"), b"k").unwrap();
        let good = data.join("keep.txt");
        for bad in [std::path::PathBuf::from("/"), std::path::PathBuf::from("/tmp"), home.clone(), tmp.path().to_path_buf()] {
            let r = super::delete_paths_in(&home, &[good.clone(), bad.clone()]);
            assert!(r.is_err(), "{}", bad.display());
        }
        assert!(good.exists(), "a refused request deletes nothing at all, even the fine items");
        let missing = super::delete_paths_in(&home, &[good.clone(), data.join("gone")]);
        assert!(missing.unwrap_err().contains("isn't there"));
        assert!(good.exists());
    }

    #[test]
    fn cloud_folder_names() {
        use super::cloud_name;
        assert_eq!(cloud_name("Box-Box"), "Box");
        assert_eq!(cloud_name("Dropbox"), "Dropbox");
        assert_eq!(cloud_name("GoogleDrive-me@gmail.com"), "Google Drive (me@gmail.com)");
        assert_eq!(cloud_name("OneDrive-Contoso"), "OneDrive (Contoso)");
        assert_eq!(cloud_name("OneDrive-Personal"), "OneDrive (Personal)");
        assert_eq!(cloud_name("SomethingNew-acct"), "SomethingNew (acct)");
    }

    #[test]
    fn cloud_places_are_found_in_the_mac_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("Library/CloudStorage");
        std::fs::create_dir_all(store.join("Box-Box")).unwrap();
        std::fs::create_dir_all(store.join("GoogleDrive-me@gmail.com")).unwrap();
        std::fs::create_dir_all(store.join(".hidden")).unwrap();
        let names: Vec<String> = super::cloud_places(tmp.path()).into_iter().map(|p| p.name).collect();
        if cfg!(target_os = "macos") {
            assert_eq!(names, ["Box", "Google Drive (me@gmail.com)"]);
        } else {
            assert!(names.is_empty(), "other systems look for Dropbox, OneDrive and the like in the home folder");
        }
    }

    use super::*;

    #[test]
    fn typed_paths() {
        let home = home();
        assert_eq!(typed_path("~").unwrap(), home);
        assert_eq!(typed_path(" ~/data/../runs/./x ").unwrap(), home.join("runs/x"));
        assert_eq!(typed_path("/a/b/..").unwrap(), PathBuf::from("/a"));
        assert_eq!(typed_path("/..").unwrap(), PathBuf::from("/"));
        assert!(typed_path("relative/path").is_err());
    }
}
