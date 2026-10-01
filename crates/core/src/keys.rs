//! Limited SSH keys for direct server-to-server transfers.
//!
//! A file server (e.g. an analysis server) gets its own key pair in
//! `~/.config/archive-helper/`. The archive server's `~/.ssh/authorized_keys`
//! gets one line for it, forced to run `archive-helper serve --restricted`
//! on one archive: that key can add and read data there, but can't open a
//! shell, forward ports, rename, or delete. Lines the app manages end with
//! `archive-helper:<label>` and can be listed and revoked.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::util;

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn config_dir() -> PathBuf {
    home().join(".config/archive-helper")
}

pub fn key_path() -> PathBuf {
    config_dir().join("transfer_key")
}

pub fn known_hosts_path() -> PathBuf {
    config_dir().join("known_hosts")
}

fn private_dir(p: &std::path::Path) -> Result<()> {
    fs::create_dir_all(p)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// This server's transfer public key, creating the key pair if needed.
pub fn ensure_keypair() -> Result<String> {
    let key = key_path();
    if !key.exists() {
        private_dir(&config_dir())?;
        let comment = format!("archive-helper@{}", util::hostname());
        let out = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", &comment, "-f"])
            .arg(&key)
            .output()
            .map_err(|e| Error::other(format!("can't run ssh-keygen: {e}")))?;
        if !out.status.success() {
            return Err(Error::other(format!("ssh-keygen failed: {}", String::from_utf8_lossy(&out.stderr).trim())));
        }
    }
    Ok(fs::read_to_string(key.with_extension("pub"))?.trim().to_string())
}

/// Remember an archive server's host key (lines in known_hosts format).
pub fn trust_hosts(lines: &[String]) -> Result<()> {
    private_dir(&config_dir())?;
    let path = known_hosts_path();
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let mut f = fs::OpenOptions::new().create(true).append(true).open(&path)?;
    for l in lines.iter().map(|l| l.trim()).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        if !existing.lines().any(|e| e.trim() == l) {
            writeln!(f, "{l}")?;
        }
    }
    Ok(())
}

/// ssh options for reaching the archive with the transfer key: no user config,
/// no agent, no prompts, and hosts checked against our own known_hosts.
pub fn transfer_ssh_args() -> Vec<String> {
    [
        "-F".to_string(),
        "/dev/null".to_string(),
        "-i".to_string(),
        key_path().display().to_string(),
        "-o".to_string(),
        "IdentitiesOnly=yes".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("UserKnownHostsFile={}", known_hosts_path().display()),
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        "-o".to_string(),
        "ConnectTimeout=20".to_string(),
    ]
    .into()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AuthorizedKey {
    pub label: String,
    pub root: String,
    /// Address the key may connect from, when limited.
    pub from: Option<String>,
    pub key: String,
}

fn authorized_keys() -> PathBuf {
    home().join(".ssh/authorized_keys")
}

fn tag(label: &str) -> String {
    format!("archive-helper:{label}")
}

fn valid(s: &str) -> bool {
    !s.is_empty() && !s.contains(['"', '\n', '\r', '\\', '\''])
}

/// The authorized_keys line for a restricted transfer key.
pub fn authorized_line(helper: &str, root: &str, key: &str, label: &str, from: Option<&str>) -> Result<String> {
    let parts: Vec<&str> = key.split_whitespace().collect();
    if parts.len() < 2 || !parts[0].starts_with("ssh-") && !parts[0].starts_with("ecdsa-") {
        return Err(Error::other("that isn't an SSH public key"));
    }
    if !valid(helper)
        || !valid(root)
        || !valid(label)
        || label.contains(char::is_whitespace)
        || from.is_some_and(|f| !valid(f) || f.contains(char::is_whitespace))
    {
        return Err(Error::InvalidPath(
            "the helper path, archive location, or label has characters that can't go in authorized_keys".into(),
        ));
    }
    let mut opts =
        format!("command=\"{helper} serve --restricted --root '{root}'\",no-pty,no-port-forwarding,no-agent-forwarding,no-X11-forwarding");
    if let Some(f) = from {
        opts.push_str(&format!(",from=\"{f}\""));
    }
    Ok(format!("{opts} {} {} {}", parts[0], parts[1], tag(label)))
}

fn write_authorized(path: &std::path::Path, content: &str) -> Result<()> {
    private_dir(path.parent().unwrap())?;
    // Keep the file private; write atomically so a failure never truncates it.
    let tmp = path.with_extension("archive-helper.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Add (or replace) the restricted line for `label`, leaving every other line as it was.
pub fn authorize(helper: &str, root: &str, key: &str, label: &str, from: Option<&str>) -> Result<()> {
    authorize_at(&authorized_keys(), helper, root, key, label, from)
}

pub fn authorize_at(file: &std::path::Path, helper: &str, root: &str, key: &str, label: &str, from: Option<&str>) -> Result<()> {
    let line = authorized_line(helper, root, key, label, from)?;
    let current = fs::read_to_string(file).unwrap_or_default();
    let t = tag(label);
    let mut out: String = current.lines().filter(|l| !l.trim_end().ends_with(&t)).map(|l| format!("{l}\n")).collect();
    out.push_str(&line);
    out.push('\n');
    write_authorized(file, &out)
}

pub fn revoke(label: &str) -> Result<bool> {
    revoke_at(&authorized_keys(), label)
}

pub fn revoke_at(file: &std::path::Path, label: &str) -> Result<bool> {
    let current = fs::read_to_string(file).unwrap_or_default();
    let t = tag(label);
    let kept: Vec<&str> = current.lines().filter(|l| !l.trim_end().ends_with(&t)).collect();
    if kept.len() == current.lines().count() {
        return Ok(false);
    }
    write_authorized(file, &kept.iter().map(|l| format!("{l}\n")).collect::<String>())?;
    Ok(true)
}

pub fn list() -> Vec<AuthorizedKey> {
    list_at(&authorized_keys())
}

pub fn list_at(file: &std::path::Path) -> Vec<AuthorizedKey> {
    let current = fs::read_to_string(file).unwrap_or_default();
    current.lines().filter_map(parse_line).collect()
}

fn parse_line(l: &str) -> Option<AuthorizedKey> {
    let label = l.trim_end().rsplit_once(' ')?.1.strip_prefix("archive-helper:")?.to_string();
    let root = l.split("--root '").nth(1)?.split('\'').next()?.to_string();
    let from = l.split("from=\"").nth(1).and_then(|r| r.split('"').next()).map(str::to_string);
    let key = l.split_whitespace().find(|w| w.starts_with("AAAA"))?.to_string();
    Some(AuthorizedKey { label, root, from, key })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorized_lines_round_trip() {
        let line = authorized_line(
            "/home/me/.local/bin/archive-helper",
            "/mnt/pool/me/archive",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample archive-helper@lab-compute",
            "lab-compute",
            Some("10.32.35.128"),
        )
        .unwrap();
        assert!(
            line.starts_with("command=\"/home/me/.local/bin/archive-helper serve --restricted --root '/mnt/pool/me/archive'\",no-pty,")
        );
        assert!(line.ends_with("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample archive-helper:lab-compute"));
        let k = parse_line(&line).unwrap();
        assert_eq!(k.label, "lab-compute");
        assert_eq!(k.root, "/mnt/pool/me/archive");
        assert_eq!(k.from.as_deref(), Some("10.32.35.128"));
        assert!(authorized_line("/h", "/root'; rm -rf ~", "ssh-ed25519 AAAA x", "l", None).is_err());
        assert!(authorized_line("/h", "/r", "not a key", "l", None).is_err());
        assert!(parse_line("ssh-ed25519 AAAAC3 someone@laptop").is_none(), "other people's keys are left alone");
    }

    #[test]
    fn authorize_and_revoke_keep_other_lines() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join(".ssh/authorized_keys");
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        let mine = "ssh-ed25519 AAAAC3NzaMine jhill@laptop\n# a comment\n";
        fs::write(&f, mine).unwrap();
        let k1 = "ssh-ed25519 AAAAC3NzaFirst archive-helper@lab-compute";
        let k2 = "ssh-ed25519 AAAAC3NzaSecond archive-helper@lab-compute";
        authorize_at(&f, "/h/archive-helper", "/mnt/a", k1, "lab-compute", None).unwrap();
        authorize_at(&f, "/h/archive-helper", "/mnt/a", k2, "lab-compute", Some("10.0.0.5")).unwrap();
        let listed = list_at(&f);
        assert_eq!(listed.len(), 1, "re-authorizing replaces the old line");
        assert_eq!(listed[0].key, "AAAAC3NzaSecond");
        assert_eq!(listed[0].from.as_deref(), Some("10.0.0.5"));
        assert!(fs::read_to_string(&f).unwrap().starts_with(mine));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(revoke_at(&f, "lab-compute").unwrap());
        assert!(!revoke_at(&f, "lab-compute").unwrap());
        assert_eq!(fs::read_to_string(&f).unwrap(), mine);
    }
}
