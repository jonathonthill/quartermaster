//! Showing ssh's password, passphrase, two-factor (2FA), and host-key prompts in the app.
//!
//! The app starts ssh with `SSH_ASKPASS` pointing at its own executable. For
//! each prompt, ssh runs that executable with the prompt text; in that mode it
//! connects back to the running app over a localhost socket (authenticated by a
//! random token), the app shows a dialog, and the answer goes back to ssh.
//! A file server signing in to an archive server asks its questions the same
//! way, relayed over the app's connection to it (see [`Askpass::ask`]).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

pub const ENV_ADDR: &str = "ARCHIVE_ASKPASS_ADDR";
pub const ENV_TOKEN: &str = "ARCHIVE_ASKPASS_TOKEN";
pub const ENV_SERVER: &str = "ARCHIVE_ASKPASS_SERVER";
/// The window whose action started this ssh, so its questions appear there.
pub const ENV_WINDOW: &str = "ARCHIVE_ASKPASS_WINDOW";

#[derive(Serialize, Deserialize)]
struct Ask {
    token: String,
    server: String,
    prompt: String,
    #[serde(default)]
    window: Option<String>,
    /// The ssh process asking (the askpass program's parent), so a cancel ends its sign-in.
    #[serde(default)]
    ssh_pid: Option<u32>,
}

#[derive(Serialize, Deserialize)]
struct Answer {
    answer: Option<String>,
}

/// If this process was started by ssh as its askpass program, answer the
/// prompt by asking the running app, then exit. Otherwise return.
pub fn run_if_askpass() {
    let (Ok(addr), Ok(token)) = (std::env::var(ENV_ADDR), std::env::var(ENV_TOKEN)) else { return };
    let prompt: String = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let server = std::env::var(ENV_SERVER).unwrap_or_default();
    let window = std::env::var(ENV_WINDOW).ok().filter(|w| !w.is_empty());
    #[cfg(unix)]
    let ssh_pid = Some(std::os::unix::process::parent_id());
    #[cfg(not(unix))]
    let ssh_pid = None;
    let code = match ask(&addr, Ask { token, server, prompt, window, ssh_pid }) {
        Some(answer) => {
            println!("{answer}");
            0
        }
        None => 1,
    };
    std::process::exit(code);
}

fn ask(addr: &str, q: Ask) -> Option<String> {
    let mut s = TcpStream::connect(addr).ok()?;
    let mut line = serde_json::to_string(&q).ok()?;
    line.push('\n');
    s.write_all(line.as_bytes()).ok()?;
    let mut reply = String::new();
    BufReader::new(s).read_line(&mut reply).ok()?;
    serde_json::from_str::<Answer>(&reply).ok()?.answer
}

#[derive(Clone, Serialize)]
pub struct PromptEvent {
    pub id: u64,
    pub server: String,
    /// The file server signing in to `server`, when it isn't this computer.
    pub via: Option<String>,
    pub prompt: String,
    /// "secret" (password), "confirm" (yes/no), or "text".
    pub kind: &'static str,
    /// The same question again: the last answer wasn't accepted.
    pub again: bool,
}

type Pending = Arc<Mutex<HashMap<u64, mpsc::Sender<Option<String>>>>>;

#[derive(Clone)]
pub struct Askpass {
    pub addr: String,
    pub token: String,
    app: AppHandle,
    pending: Pending,
    next: Arc<AtomicU64>,
    /// ssh processes whose sign-in the user cancelled. ssh asks again after a wrong answer
    /// (up to three times), and treats a cancel as a wrong answer, so its further questions
    /// are cancelled here without being shown.
    cancelled: Arc<Mutex<HashMap<u32, Instant>>>,
    /// Servers whose sign-in was just cancelled, to say so instead of "check the password".
    cancelled_servers: Arc<Mutex<HashMap<String, Instant>>>,
}

impl Askpass {
    pub fn start(app: AppHandle) -> std::io::Result<Askpass> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?.to_string();
        let token = archive_core::util::random_hex(16);
        let me = Askpass { addr, token, app, pending: Pending::default(), next: Arc::new(AtomicU64::new(1)), cancelled: Arc::default(), cancelled_servers: Arc::default() };
        let server = me.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let me = server.clone();
                std::thread::spawn(move || {
                    let _ = me.handle(conn);
                });
            }
        });
        Ok(me)
    }

    /// Show the user a question from a sign-in and wait for the answer (`None` if cancelled,
    /// or unanswered after five minutes). It appears in the named window, which is the one
    /// whose action started the sign-in; if that window is gone, or none is named, every
    /// window is asked and the first to answer wins.
    pub fn ask_in(&self, window: Option<String>, server: &str, via: Option<&str>, prompt: &str, again: bool) -> Option<String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let lower = prompt.to_lowercase();
        let kind = if lower.contains("yes/no") {
            "confirm"
        } else if lower.contains("password") || lower.contains("passphrase") {
            "secret"
        } else {
            "text"
        };
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let event = PromptEvent {
            id,
            server: server.to_string(),
            via: via.map(str::to_string),
            prompt: prompt.trim().to_string(),
            kind,
            again,
        };
        let target = window.filter(|w| self.app.get_webview_window(w).is_some());
        let _ = match &target {
            Some(w) => self.app.emit_to(w.as_str(), "auth-prompt", event),
            None => self.app.emit("auth-prompt", event),
        };
        let a = rx.recv_timeout(Duration::from_secs(300)).unwrap_or(None);
        self.pending.lock().unwrap().remove(&id);
        // Windows that were asked but didn't answer can drop the question.
        let _ = match &target {
            Some(w) => self.app.emit_to(w.as_str(), "auth-prompt-done", id),
            None => self.app.emit("auth-prompt-done", id),
        };
        a
    }

    /// Answer one prompt from an ssh started by this app.
    fn handle(&self, conn: TcpStream) -> std::io::Result<()> {
        let mut reader = BufReader::new(conn.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let Ok(q) = serde_json::from_str::<Ask>(&line) else { return Ok(()) };
        let answer = if q.token != self.token {
            None
        } else if q.ssh_pid.is_some_and(|pid| self.was_cancelled(pid)) {
            // The user already cancelled this sign-in: don't ask again.
            None
        } else {
            let answer = self.ask_in(q.window, &q.server, None, &q.prompt, false);
            let now = Instant::now();
            match &answer {
                None => {
                    if let Some(pid) = q.ssh_pid {
                        self.cancelled.lock().unwrap().insert(pid, now);
                    }
                    self.cancelled_servers.lock().unwrap().insert(q.server.clone(), now);
                }
                Some(_) => {
                    self.cancelled_servers.lock().unwrap().remove(&q.server);
                }
            }
            answer
        };
        let mut w = conn;
        let mut out = serde_json::to_string(&Answer { answer })?;
        out.push('\n');
        w.write_all(out.as_bytes())
    }

    fn was_cancelled(&self, pid: u32) -> bool {
        let mut cancelled = self.cancelled.lock().unwrap();
        cancelled.retain(|_, at| at.elapsed() < Duration::from_secs(120));
        cancelled.contains_key(&pid)
    }

    /// Whether the user cancelled signing in to this server in the last minute.
    pub fn cancelled_recently(&self, server: &str) -> bool {
        self.cancelled_servers.lock().unwrap().get(server).is_some_and(|at| at.elapsed() < Duration::from_secs(60))
    }

    /// Deliver the user's answer (`None` = cancelled) to a waiting prompt.
    pub fn answer(&self, id: u64, answer: Option<String>) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(answer);
        }
    }
}
