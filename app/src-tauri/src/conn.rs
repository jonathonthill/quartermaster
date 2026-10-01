//! Connections to servers through the system `ssh`.
//!
//! Every ssh the app starts for a server shares one authenticated connection
//! (ssh "control master"), so a password or Duo prompt is answered once, and
//! browsing and transfers can run side by side. Prompts appear in the app via
//! the askpass bridge.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use archive_core::Error;
use archive_core::remote::{Remote, SshSetup};
use archive_core::sftp::Sftp;
use serde::Serialize;

use crate::askpass::{self, Askpass};
use crate::settings::{Server, ServerKind};

pub struct Ssh {
    pub askpass: Askpass,
    /// The window this ssh setup belongs to: its sign-in questions appear there.
    pub window: String,
    pub exe: PathBuf,
    /// Folder for shared-connection sockets (not supported on Windows).
    pub control_dir: Option<PathBuf>,
    /// Hosts whose ~/.ssh/config already sets up shared connections.
    own_master: Mutex<HashMap<String, bool>>,
}

impl Ssh {
    pub fn new(askpass: Askpass, window: String) -> Ssh {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("archive-app"));
        let control_dir = control_dir();
        Ssh { askpass, window, exe, control_dir, own_master: Mutex::default() }
    }

    /// Whether the user's ssh config already shares connections to this host
    /// (ControlMaster with a ControlPath). If so, the app uses that, so a
    /// login made in Terminal also covers the app, and vice versa.
    fn user_shares_connections(&self, server: &Server) -> bool {
        let key = format!("{}:{:?}", server.destination(), server.port);
        if let Some(v) = self.own_master.lock().unwrap().get(&key) {
            return *v;
        }
        let mut cmd = std::process::Command::new("ssh");
        cmd.arg("-G");
        if let Some(p) = server.port {
            cmd.args(["-p", &p.to_string()]);
        }
        let v = cmd
            .arg(server.destination())
            .output()
            .ok()
            .map(|o| {
                let text = String::from_utf8_lossy(&o.stdout).to_lowercase();
                let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k).map(|v| v.trim().to_string()));
                let master = get("controlmaster ").unwrap_or_default();
                let path = get("controlpath ").unwrap_or_default();
                !matches!(master.as_str(), "" | "no" | "false") && !path.is_empty() && path != "none"
            })
            .unwrap_or(false);
        self.own_master.lock().unwrap().insert(key, v);
        v
    }

    /// Ask the user a sign-in question, in this setup's window.
    pub fn ask(&self, server: &str, via: Option<&str>, prompt: &str, again: bool) -> Option<String> {
        self.askpass.ask_in(Some(self.window.clone()), server, via, prompt, again)
    }

    pub fn setup(&self, server: &Server) -> SshSetup {
        let mut args = vec!["-o".into(), "ConnectTimeout=20".into(), "-o".into(), "NumberOfPasswordPrompts=3".into()];
        if let Some(dir) = self.control_dir.as_ref().filter(|_| !self.user_shares_connections(server)) {
            args.extend(
                ["ControlMaster=auto", &format!("ControlPath={}/%C", dir.display()), "ControlPersist=2h"]
                    .iter()
                    .flat_map(|o| ["-o".to_string(), o.to_string()]),
            );
        }
        let mut env = vec![
            ("SSH_ASKPASS".to_string(), self.exe.display().to_string()),
            ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
            (askpass::ENV_ADDR.to_string(), self.askpass.addr.clone()),
            (askpass::ENV_TOKEN.to_string(), self.askpass.token.clone()),
            (askpass::ENV_SERVER.to_string(), server.name.clone()),
            (askpass::ENV_WINDOW.to_string(), self.window.clone()),
        ];
        if std::env::var_os("DISPLAY").is_none() {
            // Older OpenSSH only uses SSH_ASKPASS when DISPLAY is set.
            env.push(("DISPLAY".to_string(), ":0".to_string()));
        }
        SshSetup { args, env }
    }

    /// How the server is reached as seen from other machines: the host name,
    /// user, and port that `ssh -G` resolves for this entry (aliases expanded).
    pub fn resolve(&self, server: &Server) -> (String, Option<String>, Option<u16>) {
        let mut cmd = std::process::Command::new("ssh");
        cmd.arg("-G");
        if let Some(p) = server.port {
            cmd.args(["-p", &p.to_string()]);
        }
        let text = cmd.arg(server.destination()).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k).map(|v| v.trim().to_string()));
        let host = get("hostname ").unwrap_or_else(|| server.host.clone());
        let user = get("user ").or_else(|| server.user.clone());
        let port = get("port ").and_then(|p| p.parse().ok()).filter(|p| *p != 22);
        (host, user, port)
    }

    /// This computer's known_hosts lines for a host, to pass on to a file server.
    pub fn known_host_lines(&self, host: &str, port: Option<u16>) -> Vec<String> {
        let name = match port {
            Some(p) => format!("[{host}]:{p}"),
            None => host.to_string(),
        };
        std::process::Command::new("ssh-keygen")
            .args(["-F", &name])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A short, private folder for control sockets (socket paths are length-limited).
fn control_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let uid = unsafe { libc::getuid() };
        let dir = PathBuf::from(format!("/tmp/archive-ssh-{uid}"));
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok()?;
        Some(dir)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ConnInfo {
    pub kind: ServerKind,
    pub connected: bool,
    /// What's missing: "helper" (not installed), "update" (the helper is out of
    /// date), or "archive" (none at this location yet).
    pub needs: Option<String>,
    pub helper: Option<String>,
    pub os: Option<String>,
    pub free_bytes: Option<u64>,
    pub message: Option<String>,
    /// The server's host name, to recognize the same machine under two entries.
    pub host: Option<String>,
}

#[derive(Default)]
pub struct Conns {
    map: Mutex<HashMap<String, Arc<Mutex<Remote>>>>,
    /// SFTP servers' sessions, by server.
    sftps: Mutex<HashMap<String, Arc<Mutex<Sftp>>>>,
    /// One lock per server while its connection is being made, so two windows connecting to the
    /// same server at once share one sign-in instead of each asking for a password.
    starting: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Archives whose background maintenance was started this session.
    maintained: Mutex<std::collections::HashSet<String>>,
}

fn helper_missing(e: &Error) -> bool {
    let m = e.to_string();
    m.contains("not found") || m.contains("No such file")
}

/// The server's helper is too old (or too new) for this app.
fn helper_outdated(e: &Error) -> bool {
    e.to_string().contains("update the helper")
}

fn needs(server: &Server, what: &str) -> ConnInfo {
    ConnInfo {
        kind: server.kind,
        connected: true,
        needs: Some(what.into()),
        helper: None,
        os: None,
        free_bytes: None,
        message: None,
        host: None,
    }
}

impl Conns {
    /// Connect (or reuse the connection) and describe the server.
    pub fn connect(&self, ssh: &Ssh, server: &Server) -> ConnInfo {
        match server.kind {
            ServerKind::Files => match self.remote(ssh, server) {
                Ok(r) => {
                    let r = r.lock().unwrap();
                    let h = r.hello();
                    ConnInfo {
                        kind: server.kind,
                        connected: true,
                        needs: None,
                        helper: Some(h.helper.clone()),
                        os: Some(h.os.clone()),
                        free_bytes: h.free_bytes,
                        message: None,
                        host: Some(h.host.clone()),
                    }
                }
                Err(e) if helper_missing(&e) => needs(server, "helper"),
                Err(e) if helper_outdated(&e) => needs(server, "update"),
                Err(e) => failed(server, e, ssh.askpass.cancelled_recently(&server.name)),
            },
            ServerKind::Sftp => match self.sftp(ssh, server) {
                Ok(_) => ConnInfo {
                    kind: server.kind,
                    connected: true,
                    needs: None,
                    helper: Some("SFTP".into()),
                    os: None,
                    free_bytes: None,
                    message: None,
                    host: Some(server.host.clone()),
                },
                Err(e) => failed(server, e, ssh.askpass.cancelled_recently(&server.name)),
            },
            ServerKind::Archive => match self.remote(ssh, server) {
                Ok(r) => {
                    let mut r = r.lock().unwrap();
                    // Catch up on any maintenance (PAR2, checks, cleanup) the first
                    // time we reach this archive in a session. A run with nothing to
                    // do takes about a second; the server skips it if one is going.
                    if r.hello().archive_id.is_some() && self.maintained.lock().unwrap().insert(server.id.clone()) {
                        let _ = r.kick_worker();
                    }
                    let h = r.hello();
                    ConnInfo {
                        kind: server.kind,
                        connected: true,
                        needs: h.archive_id.is_none().then(|| "archive".to_string()),
                        helper: Some(h.helper.clone()),
                        os: Some(h.os.clone()),
                        free_bytes: h.free_bytes,
                        message: None,
                        host: Some(h.host.clone()),
                    }
                }
                Err(e) if helper_missing(&e) => needs(server, "helper"),
                Err(e) if helper_outdated(&e) => needs(server, "update"),
                Err(e) => failed(server, e, ssh.askpass.cancelled_recently(&server.name)),
            },
        }
    }

    /// The host name the server reported, connecting if needed.
    pub fn host_of(&self, ssh: &Ssh, server: &Server) -> archive_core::Result<String> {
        let r = self.remote(ssh, server)?;
        let host = r.lock().unwrap().hello().host.clone();
        Ok(host)
    }

    fn remote(&self, ssh: &Ssh, server: &Server) -> archive_core::Result<Arc<Mutex<Remote>>> {
        if let Some(r) = self.map.lock().unwrap().get(&server.id) {
            return Ok(r.clone());
        }
        let gate = self.starting.lock().unwrap().entry(server.id.clone()).or_default().clone();
        let _starting = gate.lock().unwrap();
        // Another window may have connected while this one waited.
        if let Some(r) = self.map.lock().unwrap().get(&server.id) {
            return Ok(r.clone());
        }
        let r = Arc::new(Mutex::new(Remote::ssh_with(&server.target(), &ssh.setup(server))?));
        self.map.lock().unwrap().insert(server.id.clone(), r.clone());
        Ok(r)
    }

    /// The servers this window is connected to right now.
    pub fn open_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.map.lock().unwrap().keys().cloned().collect();
        ids.extend(self.sftps.lock().unwrap().keys().cloned());
        ids
    }

    pub fn forget(&self, id: &str) {
        self.map.lock().unwrap().remove(id);
        self.sftps.lock().unwrap().remove(id);
    }

    /// The SFTP session for a server, made on first use (one sign-in at a time, as for others).
    fn sftp(&self, ssh: &Ssh, server: &Server) -> archive_core::Result<Arc<Mutex<Sftp>>> {
        if let Some(s) = self.sftps.lock().unwrap().get(&server.id) {
            return Ok(s.clone());
        }
        let gate = self.starting.lock().unwrap().entry(server.id.clone()).or_default().clone();
        let _starting = gate.lock().unwrap();
        if let Some(s) = self.sftps.lock().unwrap().get(&server.id) {
            return Ok(s.clone());
        }
        let mut sftp = Sftp::ssh(&server.destination(), server.port, &ssh.setup(server))?;
        sftp.read_back = server.read_back;
        let s = Arc::new(Mutex::new(sftp));
        self.sftps.lock().unwrap().insert(server.id.clone(), s.clone());
        Ok(s)
    }

    /// Run `f` on an SFTP server's session, reconnecting once if it dropped.
    pub fn with_sftp<T>(&self, ssh: &Ssh, server: &Server, f: impl Fn(&mut Sftp) -> archive_core::Result<T>) -> archive_core::Result<T> {
        let s = self.sftp(ssh, server)?;
        let res = f(&mut s.lock().unwrap());
        match res {
            Err(e) if e.is_lost() => {
                self.sftps.lock().unwrap().remove(&server.id);
                let s = self.sftp(ssh, server)?;
                let mut guard = s.lock().unwrap();
                f(&mut guard)
            }
            other => other,
        }
    }

    /// Run `f` on the server's browsing connection, reconnecting once if it dropped.
    pub fn with<T>(&self, ssh: &Ssh, server: &Server, f: impl Fn(&mut Remote) -> archive_core::Result<T>) -> archive_core::Result<T> {
        let r = self.remote(ssh, server)?;
        let res = f(&mut r.lock().unwrap());
        match res {
            Err(e) if e.is_lost() => {
                self.forget(&server.id);
                let r = self.remote(ssh, server)?;
                let mut guard = r.lock().unwrap();
                f(&mut guard)
            }
            other => other,
        }
    }
}

fn failed(server: &Server, e: Error, cancelled: bool) -> ConnInfo {
    let msg = e.to_string().replace("lost the connection to the server: ", "");
    let friendly = if cancelled && msg.contains("Permission denied") {
        format!("Signing in to {} was cancelled.", server.name)
    } else if msg.contains("Permission denied") {
        format!("Couldn't sign in to {}. Check the username and password.", server.host)
    } else if msg.contains("Could not resolve") || msg.contains("nodename nor servname") {
        format!("Can't find {}. Check the address (and that you're on the VPN).", server.host)
    } else if msg.contains("timed out") || msg.contains("Operation timed out") {
        format!("{} didn't answer. Are you on the VPN?", server.host)
    } else {
        msg
    };
    ConnInfo {
        kind: server.kind,
        connected: false,
        needs: None,
        helper: None,
        os: None,
        free_bytes: None,
        message: Some(friendly),
        host: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two threads asking for the same server's connection at once take turns; the second
    /// finds the first's connection instead of making its own.
    #[test]
    fn a_second_caller_waits_for_the_first_connection() {
        let conns = Conns::default();
        let gate = conns.starting.lock().unwrap().entry("s".into()).or_default().clone();
        let held = gate.lock().unwrap();
        let waiting = std::thread::scope(|scope| {
            let t = scope.spawn(|| conns.starting.lock().unwrap().entry("s".into()).or_default().clone().try_lock().is_err());
            t.join().unwrap()
        });
        drop(held);
        assert!(waiting, "the same server shares one lock");
        let other = conns.starting.lock().unwrap().entry("t".into()).or_default().clone();
        assert!(other.try_lock().is_ok(), "a different server is not held up");
    }
}
