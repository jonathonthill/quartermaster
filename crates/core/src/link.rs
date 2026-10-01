//! Signed-in links: a file server's own SSH connection to an archive server,
//! opened with the user's sign-in (password, two-factor code, and so on) and kept open so
//! transfer jobs on the file server reach the archive directly. This works
//! whatever sign-in the archive server requires, including servers that don't
//! accept SSH keys.
//!
//! [`open`] runs `ssh -f -N` as a connection-sharing master (ControlMaster)
//! whose socket sits in a private folder. ssh asks its questions through
//! `SSH_ASKPASS`, which points at this same helper: started that way, it hands
//! the prompt over a Unix socket to the `files` session running the sign-in,
//! which relays it to the app and the user's answer back ([`askpass_main`]).
//! Nothing is saved; the answer only passes through. Jobs then run ssh with
//! `ControlMaster=no` over the socket, needing no sign-in of their own. The
//! connection closes by itself [`PERSIST_SECS`] after the last job using it ends,
//! or when the server restarts or the network drops; jobs then wait until the
//! user signs in again.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Environment variables that tell the helper it was started as ssh's askpass.
pub const ENV_SOCK: &str = "ARCHIVE_HELPER_ASKPASS";
pub const ENV_TOKEN: &str = "ARCHIVE_HELPER_ASKPASS_TOKEN";
/// How long a link stays open with nothing using it.
pub const PERSIST_SECS: u64 = 2 * 3600;

/// The archive server a link reaches, as this file server should address it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct LinkTarget {
    pub host: String,
    pub port: Option<u16>,
    pub user: Option<String>,
}

impl LinkTarget {
    /// `user@host[:port]`, for messages.
    pub fn label(&self) -> String {
        let mut s = match self.user.as_deref().filter(|u| !u.is_empty()) {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        };
        if let Some(p) = self.port {
            s.push_str(&format!(":{p}"));
        }
        s
    }
}

#[cfg(unix)]
use crate::remote::ssh_program as ssh;

#[cfg(unix)]
fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// The private folder for link sockets. Socket paths are limited to about 100
/// bytes, so a long home folder falls back to a folder under /tmp.
#[cfg(unix)]
fn dir() -> Result<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let uid = unsafe { libc::getuid() };
    let mut d = home().join(".local/share/archive-helper/links");
    if d.as_os_str().len() > 78 {
        d = PathBuf::from(format!("/tmp/archive-links-{uid}"));
    }
    std::fs::create_dir_all(&d)?;
    let m = std::fs::symlink_metadata(&d)?;
    if !m.is_dir() || m.uid() != uid {
        return Err(Error::other(format!("{} isn't a folder owned by you, so it can't hold sign-ins", d.display())));
    }
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700))?;
    Ok(d)
}

#[cfg(unix)]
fn socket(t: &LinkTarget) -> Result<PathBuf> {
    let name = crate::hash::Digest::of(t.label().as_bytes()).to_hex();
    Ok(dir()?.join(format!("{}.sock", &name[..16])))
}

/// Options every link command shares: no user ssh config (the app has already
/// resolved the host), our own known_hosts, and the link's socket.
#[cfg(unix)]
fn common(sock: &Path) -> Vec<String> {
    [
        "-F".to_string(),
        "/dev/null".to_string(),
        "-o".to_string(),
        format!("ControlPath={}", sock.display().to_string().replace('%', "%%")),
        "-o".to_string(),
        format!("UserKnownHostsFile={}", crate::keys::known_hosts_path().display()),
        "-o".to_string(),
        "ConnectTimeout=20".to_string(),
    ]
    .into()
}

#[cfg(unix)]
fn destination(t: &LinkTarget) -> Vec<String> {
    let mut a = Vec::new();
    if let Some(p) = t.port {
        a.extend(["-p".to_string(), p.to_string()]);
    }
    if let Some(u) = t.user.as_deref().filter(|u| !u.is_empty()) {
        a.extend(["-l".to_string(), u.to_string()]);
    }
    a.push(t.host.clone());
    a
}

/// Is the link open and working?
#[cfg(unix)]
pub fn check(t: &LinkTarget) -> bool {
    use std::process::{Command, Stdio};
    let Ok(sock) = socket(t) else { return false };
    if std::fs::symlink_metadata(&sock).is_err() {
        return false;
    }
    Command::new(ssh())
        .args(common(&sock))
        .args(["-O", "check"])
        .args(destination(t))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Close the link now (running jobs over it stop and wait for a new sign-in).
#[cfg(unix)]
pub fn close(t: &LinkTarget) -> Result<()> {
    use std::process::{Command, Stdio};
    let sock = socket(t)?;
    if std::fs::symlink_metadata(&sock).is_ok() {
        let _ = Command::new(ssh())
            .args(common(&sock))
            .args(["-O", "exit"])
            .args(destination(t))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_file(&sock);
    }
    Ok(())
}

/// ssh options for a job's session over the link (placed before `user@host`).
/// They never prompt: without the link, the session fails at once.
#[cfg(unix)]
pub fn session_args(t: &LinkTarget) -> Result<Vec<String>> {
    let mut a = common(&socket(t)?);
    a.extend(["-o", "ControlMaster=no", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes"].map(String::from));
    Ok(a)
}

#[derive(Serialize, Deserialize)]
struct Ask {
    token: String,
    prompt: String,
}

#[derive(Serialize, Deserialize)]
struct Answer {
    answer: Option<String>,
}

/// Sign in to `t` from this server, unless the link is already open. Each
/// question ssh asks (password, two-factor code, an unknown host key) goes to `ask`;
/// `None` cancels. `askpass` is this helper's executable.
#[cfg(unix)]
pub fn open(t: &LinkTarget, askpass: &Path, ask: &mut dyn FnMut(&str) -> Option<String>) -> Result<()> {
    use std::os::unix::net::UnixListener;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    if check(t) {
        return Ok(());
    }
    let sock = socket(t)?;
    // A socket left by a link that has ended.
    let _ = std::fs::remove_file(&sock);
    let d = dir()?;
    let tag = crate::util::random_hex(6);
    let prompt_sock = d.join(format!("ask-{tag}.sock"));
    let log = d.join(format!("ask-{tag}.log"));
    let listener = UnixListener::bind(&prompt_sock)?;
    listener.set_nonblocking(true)?;
    let token = crate::util::random_hex(16);

    let mut cmd = Command::new(ssh());
    cmd.args(common(&sock))
        .args(["-o", "ControlMaster=yes", "-o", &format!("ControlPersist={PERSIST_SECS}")])
        .args(["-o", "ServerAliveInterval=30", "-o", "ServerAliveCountMax=10"])
        .args(["-o", "StrictHostKeyChecking=ask", "-o", "NumberOfPasswordPrompts=3", "-f", "-N"])
        .args(destination(t))
        .env("SSH_ASKPASS", askpass)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env(ENV_SOCK, &prompt_sock)
        .env(ENV_TOKEN, &token)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log)?);
    if std::env::var_os("DISPLAY").is_none() {
        // Older OpenSSH uses SSH_ASKPASS only when DISPLAY is set and there's no terminal.
        cmd.env("DISPLAY", ":0");
    }
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let cleanup = || {
        let _ = std::fs::remove_file(&prompt_sock);
        let _ = std::fs::remove_file(&log);
    };
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            cleanup();
            return Err(Error::other(format!("can't run ssh: {e}")));
        }
    };

    // Answer ssh's questions until it signs in (and goes to the background) or gives up.
    let deadline = Instant::now() + Duration::from_secs(15 * 60);
    let mut cancelled = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(_) => break None,
        }
        match listener.accept() {
            Ok((conn, _)) => {
                if !relay(conn, &token, ask) {
                    // Cancelled: stop now rather than let ssh ask again.
                    cancelled = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
    };
    let stderr = std::fs::read_to_string(&log).unwrap_or_default();
    cleanup();
    if status.is_some_and(|s| s.success()) && check(t) {
        return Ok(());
    }
    Err(Error::other(sign_in_error(t, &stderr, cancelled, status.is_none() && !cancelled)))
}

/// Pass one question from ssh to the user. Returns false if it was cancelled.
#[cfg(unix)]
fn relay(conn: std::os::unix::net::UnixStream, token: &str, ask: &mut dyn FnMut(&str) -> Option<String>) -> bool {
    use std::io::{BufRead, BufReader, Write};
    let _ = conn.set_nonblocking(false);
    let mut line = String::new();
    let Ok(read) = conn.try_clone() else { return false };
    if BufReader::new(read).read_line(&mut line).is_err() {
        return false;
    }
    let answer = match serde_json::from_str::<Ask>(&line) {
        Ok(q) if q.token == token => ask(q.prompt.trim()),
        _ => None,
    };
    let answered = answer.is_some();
    let mut reply = serde_json::to_string(&Answer { answer }).unwrap_or_default();
    reply.push('\n');
    let _ = (&conn).write_all(reply.as_bytes());
    answered
}

#[cfg(unix)]
fn sign_in_error(t: &LinkTarget, stderr: &str, cancelled: bool, timed_out: bool) -> String {
    let host = &t.host;
    let here = crate::util::hostname();
    let detail = stderr.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("Warning:")).next_back().unwrap_or("");
    if cancelled {
        format!("The sign-in to {host} was cancelled.")
    } else if timed_out {
        format!("The sign-in to {host} took too long and was stopped.")
    } else if stderr.contains("Permission denied") {
        format!("{host} didn't accept the sign-in from {here}. Check the password and try again.")
    } else if stderr.contains("Host key verification failed") || stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        format!("{here} couldn't confirm it was talking to the real {host}, so it didn't sign in.")
    } else if stderr.contains("Could not resolve hostname") || stderr.contains("Name or service not known") {
        format!("{here} can't find {host} by that name.")
    } else if ["timed out", "Connection refused", "No route to host", "Network is unreachable"].iter().any(|m| stderr.contains(m)) {
        format!("{here} can't reach {host} over the network ({detail}).")
    } else if detail.is_empty() {
        format!("{here} couldn't sign in to {host}.")
    } else {
        format!("{here} couldn't sign in to {host}: {detail}")
    }
}

/// If ssh started this process to ask a question (see [`open`]), pass it to
/// the session running the sign-in, print the answer, and return the exit
/// code. Otherwise return `None`.
pub fn askpass_main() -> Option<u8> {
    let sock = std::env::var_os(ENV_SOCK)?;
    let token = std::env::var(ENV_TOKEN).unwrap_or_default();
    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    #[cfg(unix)]
    {
        use std::io::{BufRead, BufReader, Write};
        let ask = || -> Option<String> {
            let mut s = std::os::unix::net::UnixStream::connect(&sock).ok()?;
            let mut line = serde_json::to_string(&Ask { token, prompt }).ok()?;
            line.push('\n');
            s.write_all(line.as_bytes()).ok()?;
            let mut reply = String::new();
            BufReader::new(s).read_line(&mut reply).ok()?;
            serde_json::from_str::<Answer>(&reply).ok()?.answer
        };
        match ask() {
            Some(a) => {
                println!("{a}");
                Some(0)
            }
            None => Some(1),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (sock, token, prompt);
        Some(1)
    }
}

#[cfg(not(unix))]
pub fn check(_: &LinkTarget) -> bool {
    false
}

#[cfg(not(unix))]
pub fn close(_: &LinkTarget) -> Result<()> {
    Ok(())
}

#[cfg(not(unix))]
pub fn session_args(_: &LinkTarget) -> Result<Vec<String>> {
    Err(Error::other("signed-in links need a Linux, BSD, or macOS server"))
}

#[cfg(not(unix))]
pub fn open(_: &LinkTarget, _: &Path, _: &mut dyn FnMut(&str) -> Option<String>) -> Result<()> {
    Err(Error::other("signed-in links need a Linux, BSD, or macOS server"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        let t = LinkTarget { host: "storage.example.edu".into(), port: None, user: Some("alex".into()) };
        assert_eq!(t.label(), "alex@storage.example.edu");
        let t = LinkTarget { host: "h".into(), port: Some(2222), user: None };
        assert_eq!(t.label(), "h:2222");
    }
}
