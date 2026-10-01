//! Thin wrapper around the `par2` command (par2cmdline or par2cmdline-turbo).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verify {
    /// All data matches the recovery set.
    Ok,
    /// Damage found, and there is enough recovery data to repair it.
    Repairable,
    /// Damage found beyond what the recovery data can repair.
    Unrepairable,
}

#[derive(Clone, Debug)]
pub struct Par2 {
    exe: PathBuf,
}

impl Par2 {
    /// Use `explicit` if given, else `$ARCHIVE_PAR2`, else `par2` on PATH.
    pub fn find(explicit: Option<&Path>) -> Result<Par2> {
        let exe = explicit
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("ARCHIVE_PAR2").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("par2"));
        let p = Par2 { exe };
        p.version()?;
        Ok(p)
    }

    /// Find par2 for an archive: the configured path, then `<root>/tools/par2`,
    /// `~/bin/par2`, `~/.local/bin/par2`, `/usr/local/bin/par2`, then PATH.
    /// Cron runs with a minimal PATH, so the explicit locations matter.
    pub fn discover(configured: Option<&str>, root: &Path) -> Result<Par2> {
        if let Some(c) = configured {
            return Par2::find(Some(Path::new(c)));
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut candidates = vec![root.join("tools/par2")];
        if let Some(h) = &home {
            candidates.push(h.join("bin/par2"));
            candidates.push(h.join(".local/bin/par2"));
        }
        candidates.push(PathBuf::from("/usr/local/bin/par2"));
        for c in candidates {
            if c.is_file() {
                if let Ok(p) = Par2::find(Some(&c)) {
                    return Ok(p);
                }
            }
        }
        Par2::find(None)
    }

    pub fn path(&self) -> &Path {
        &self.exe
    }

    pub fn version(&self) -> Result<String> {
        let out =
            Command::new(&self.exe).arg("--version").output().map_err(|e| Error::Par2(format!("can't run {}: {e}", self.exe.display())))?;
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text.lines().next().unwrap_or("par2").trim().to_string())
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Result<i32> {
        let out = Command::new(&self.exe)
            .current_dir(dir)
            .args(args)
            .output()
            .map_err(|e| Error::Par2(format!("can't run {}: {e}", self.exe.display())))?;
        match out.status.code() {
            Some(c) => Ok(c),
            None => Err(Error::Par2(format!("par2 was killed: {}", String::from_utf8_lossy(&out.stderr)))),
        }
    }

    fn split(file: &Path) -> Result<(&Path, &str)> {
        let dir = file.parent().unwrap_or(Path::new("."));
        let name = file.file_name().and_then(|n| n.to_str()).ok_or_else(|| Error::Par2(format!("bad file name {}", file.display())))?;
        Ok((dir, name))
    }

    /// Create recovery files `<file>.par2` and `<file>.vol*.par2` beside `file`.
    pub fn create(&self, file: &Path, redundancy_percent: u32) -> Result<()> {
        let (dir, name) = Self::split(file)?;
        remove_recovery_files(file)?;
        let index = format!("{name}.par2");
        let r = format!("-r{redundancy_percent}");
        match self.run(dir, &["create", "-q", "-q", &r, &index, "--", name])? {
            0 => Ok(()),
            c => {
                let _ = remove_recovery_files(file);
                Err(Error::Par2(format!("par2 create failed for {name} (exit {c})")))
            }
        }
    }

    pub fn verify(&self, file: &Path) -> Result<Verify> {
        let (dir, name) = Self::split(file)?;
        let index = format!("{name}.par2");
        if !dir.join(&index).exists() {
            return Err(Error::Par2(format!("no recovery data for {name}")));
        }
        match self.run(dir, &["verify", "-q", "-q", &index])? {
            0 => Ok(Verify::Ok),
            1 => Ok(Verify::Repairable),
            2 | 4 => Ok(Verify::Unrepairable),
            c => Err(Error::Par2(format!("par2 verify failed for {name} (exit {c})"))),
        }
    }

    /// Repair `file` in place. par2 keeps the damaged original as `<file>.1`;
    /// it is removed only after the repaired file verifies.
    pub fn repair(&self, file: &Path) -> Result<bool> {
        let (dir, name) = Self::split(file)?;
        let index = format!("{name}.par2");
        let code = self.run(dir, &["repair", "-q", "-q", &index])?;
        if code != 0 {
            return Ok(false);
        }
        if self.verify(file)? != Verify::Ok {
            return Ok(false);
        }
        for n in 1..10 {
            let backup = dir.join(format!("{name}.{n}"));
            if backup.exists() {
                fs::remove_file(backup)?;
            }
        }
        Ok(true)
    }
}

/// The recovery files belonging to `file`.
pub fn recovery_files(file: &Path) -> Result<Vec<PathBuf>> {
    let (dir, name) = Par2::split(file)?;
    let prefix = format!("{name}.vol");
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else { return Ok(out) };
    for e in rd.flatten() {
        let n = e.file_name();
        let Some(n) = n.to_str() else { continue };
        if n == format!("{name}.par2") || (n.starts_with(&prefix) && n.ends_with(".par2")) {
            out.push(e.path());
        }
    }
    out.sort();
    Ok(out)
}

pub fn remove_recovery_files(file: &Path) -> Result<()> {
    for p in recovery_files(file)? {
        fs::remove_file(p)?;
    }
    Ok(())
}
