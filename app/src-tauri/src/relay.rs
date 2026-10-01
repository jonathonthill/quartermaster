//! Relaying a file server's transfer through this computer, for file servers
//! that can't reach an archive themselves or can't leave jobs running.
//!
//! The job still runs on the file server, which reads, hashes, compresses,
//! and in Move mode deletes each file after the archive has verified it. This
//! computer starts it over ssh (`archive-helper job run`) and joins its stdin
//! and stdout to an archive session of its own, so only the protocol's
//! compressed bytes pass through here. If either connection drops, the job
//! pauses on the server and is started again here; finished files are kept.

use std::collections::HashSet;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use archive_core::remote::{DEFAULT_HELPER, helper_command, shell_quote, ssh_command};

use crate::conn::{Conns, Ssh};
use crate::settings::Server;

/// Relay a job until it finishes, restarting it after dropped connections.
/// Does nothing if this job is already being relayed.
pub fn spawn(ssh: Arc<Ssh>, conns: Arc<Conns>, active: Arc<Mutex<HashSet<String>>>, files: Server, archive: Server, id: String) {
    let key = format!("{}:{id}", files.id);
    if !active.lock().unwrap().insert(key.clone()) {
        return;
    }
    std::thread::spawn(move || {
        let _awake = KeepAwake::start();
        let (mut tries, mut last_done) = (0, 0);
        loop {
            let _ = pipe(&ssh, &files, &archive, &id);
            let st = conns.with(&ssh, &files, |r| r.job_list(false)).ok().and_then(|l| l.into_iter().find(|s| s.id == id));
            let unfinished = match &st {
                Some(s) => matches!(s.state.as_str(), "interrupted" | "queued" | "running"),
                None => true, // the file server is out of reach for now
            };
            if !unfinished {
                break;
            }
            if let Some(s) = &st {
                if s.done > last_done {
                    (tries, last_done) = (0, s.done);
                }
            }
            tries += 1;
            if tries > 20 {
                // Give up for now; connecting to the file server again resumes it.
                break;
            }
            std::thread::sleep(Duration::from_secs(15));
        }
        active.lock().unwrap().remove(&key);
    });
}

/// Run the job once, joined to the archive, until it ends or a connection drops.
fn pipe(ssh: &Ssh, files: &Server, archive: &Server, id: &str) -> Result<(), String> {
    let target = archive.target();
    let mut arc = ssh_command(&target.host, target.port, &ssh.setup(archive), &helper_command(&target))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("can't run ssh: {e}"))?;
    let job_cmd = format!("{DEFAULT_HELPER} job run --id {}", shell_quote(id));
    let mut job = match ssh_command(&files.destination(), files.port, &ssh.setup(files), &job_cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(j) => j,
        Err(e) => {
            let _ = arc.kill();
            let _ = arc.wait();
            return Err(format!("can't run ssh: {e}"));
        }
    };
    let (mut jo, mut ji) = (job.stdout.take().unwrap(), job.stdin.take().unwrap());
    let (mut ao, mut ai) = (arc.stdout.take().unwrap(), arc.stdin.take().unwrap());
    let mut je = job.stderr.take().unwrap();
    // Job → archive, and archive → job. When one side ends, closing its
    // partner's input ends the other.
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut jo, &mut ai);
    });
    let down = std::thread::spawn(move || {
        let _ = std::io::copy(&mut ao, &mut ji);
    });
    let errors = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = je.read_to_string(&mut s);
        s
    });
    let status = job.wait();
    let _ = up.join();
    if !exits_within(&mut arc, Duration::from_secs(30)) {
        let _ = arc.kill();
        let _ = arc.wait();
    }
    let _ = down.join();
    let stderr = errors.join().unwrap_or_default();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => Err(stderr.trim().to_string()),
    }
}

/// Wait up to `limit` for a process to exit by itself.
fn exits_within(c: &mut Child, limit: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < limit {
        if let Ok(Some(_)) = c.try_wait() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Keeps a Mac from idle-sleeping while a relay runs (it can't continue asleep).
struct KeepAwake(Option<Child>);

impl KeepAwake {
    fn start() -> KeepAwake {
        #[cfg(target_os = "macos")]
        {
            let pid = std::process::id().to_string();
            let c = Command::new("caffeinate")
                .args(["-i", "-w", &pid])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok();
            KeepAwake(c)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = Command::new::<&str>;
            KeepAwake(None)
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
