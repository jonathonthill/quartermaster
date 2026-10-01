//! Saved servers and preferences, stored as JSON in the app's config folder.

use std::fs;
use std::path::Path;

use archive_core::remote::SshTarget;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ServerKind {
    /// An archive server (compressed, checksummed, PAR2-protected).
    #[default]
    Archive,
    /// A plain file server, such as an analysis server.
    Files,
    /// A server reached by SFTP alone, without the helper (copies are checked by size).
    Sftp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Server {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: ServerKind,
    /// Host name or an alias from ~/.ssh/config.
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    /// Blank to use the user from ~/.ssh/config (or the local user name).
    #[serde(default)]
    pub user: Option<String>,
    /// Archive location (archives) or starting folder (file servers).
    #[serde(default)]
    pub root: String,
    /// Archives only: file servers may use a limited SSH key here instead of
    /// signing in (only works if the archive server accepts keys).
    #[serde(default)]
    pub allow_keys: bool,
    /// File servers only: send and retrieve through this computer instead of
    /// directly (for servers that can't reach archives or keep jobs running).
    #[serde(default)]
    pub relay: bool,
    /// SFTP servers only: read each upload back and compare checksums (slower, but checked).
    #[serde(default)]
    pub read_back: bool,
    /// A one-off connection: usable until the app quits, but never written to the settings file.
    #[serde(default)]
    pub temporary: bool,
}

impl Server {
    /// The `[user@]host` argument for ssh.
    pub fn destination(&self) -> String {
        match self.user.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            Some(u) => format!("{u}@{}", self.host.trim()),
            None => self.host.trim().to_string(),
        }
    }

    pub fn target(&self) -> SshTarget {
        SshTarget {
            host: self.destination(),
            port: self.port,
            root: self.root.trim().to_string(),
            helper: archive_core::remote::DEFAULT_HELPER.to_string(),
            files: self.kind == ServerKind::Files,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub servers: Vec<Server>,
    /// "copy" or "move".
    #[serde(default = "default_mode")]
    pub default_mode: String,
    #[serde(default)]
    pub show_hidden: bool,
    /// File servers set up to send to (and retrieve from) archives directly.
    #[serde(default)]
    pub routes: Vec<Route>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Route {
    pub files: String,
    pub archive: String,
}

fn default_mode() -> String {
    "copy".into()
}

impl Default for Settings {
    fn default() -> Self {
        Settings { servers: Vec::new(), default_mode: default_mode(), show_hidden: false, routes: Vec::new() }
    }
}

pub fn load(path: &Path) -> Settings {
    fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Write the settings, leaving out temporary servers (and keys set up for them).
pub fn save(path: &Path, s: &Settings) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut kept = s.clone();
    kept.servers.retain(|x| !x.temporary);
    let saved = |id: &str| kept.servers.iter().any(|x| x.id == id);
    let routes = kept.routes.iter().filter(|r| saved(&r.files) && saved(&r.archive)).cloned().collect();
    kept.routes = routes;
    archive_core::util::atomic_write(path, &serde_json::to_vec_pretty(&kept)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_servers_are_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let server = |id: &str, temporary| Server {
            id: id.into(),
            name: id.into(),
            kind: ServerKind::Files,
            host: format!("{id}.example.edu"),
            port: None,
            user: None,
            root: "~".into(),
            allow_keys: false,
            relay: false,
            read_back: false,
            temporary,
        };
        let mut s = Settings::default();
        s.servers = vec![server("kept", false), server("once", true)];
        s.routes = vec![Route { files: "once".into(), archive: "kept".into() }];
        save(&path, &s).unwrap();
        let back = load(&path);
        assert_eq!(back.servers.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["kept"]);
        assert!(back.routes.is_empty(), "a key for a temporary server isn't kept either");
        assert!(!fs::read_to_string(&path).unwrap().contains("once"));
    }
}
