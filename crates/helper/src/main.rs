//! `archive-helper`: runs on archive servers and file servers. Clients start
//! `archive-helper serve` (archives) or `archive-helper files` (file servers)
//! over SSH; the app and finished uploads start `archive-helper worker` for
//! PAR2, integrity checks, and cleanup; file servers run transfers as
//! `archive-helper job run`. When a file server signs in to an archive server,
//! ssh runs this program again to ask its questions (see `archive_core::link`).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use archive_core::proto::PROTOCOL_VERSION;
use archive_core::serve::{self, ServeOptions};
use archive_core::worker::{self, WorkerOptions};
use clap::{Parser, Subcommand};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(name = "archive-helper", version = long_version(), about = "Server-side helper for the Archive tool")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

fn long_version() -> &'static str {
    Box::leak(format!("{VERSION} (protocol {PROTOCOL_VERSION}, {}-{})", std::env::consts::OS, std::env::consts::ARCH).into_boxed_str())
}

#[derive(Subcommand)]
enum Cmd {
    /// Speak the archive protocol on stdin/stdout (started by clients over SSH).
    Serve {
        #[arg(long)]
        root: String,
        /// Allow only reading and adding data (for server-to-server keys).
        #[arg(long)]
        restricted: bool,
    },
    /// Serve an ordinary file system (a file server) on stdin/stdout: browse,
    /// search, and run transfer jobs to and from archives.
    Files {
        /// Folder to show first.
        #[arg(long, default_value = "~")]
        start: String,
    },
    /// Transfer jobs on this server.
    Job {
        #[command(subcommand)]
        action: JobAction,
    },
    /// Run one maintenance pass: seal, PAR2-protect, check, purge, snapshot.
    Worker {
        #[arg(long)]
        root: String,
        /// Stop starting new PAR2 or check work after this many minutes.
        #[arg(long, default_value_t = 50)]
        budget_minutes: u64,
        /// Do the once-a-day steps (purge, snapshot) even if done today.
        #[arg(long)]
        force_daily: bool,
        /// Check packs not verified in this many days (0 checks everything now).
        #[arg(long, default_value_t = 90)]
        scrub_days: u32,
    },
    /// Import bundles made by the earlier move2storage tool (`<part>.bundle.tar`).
    /// Each is checked, unpacked into a staging folder, and moved into the
    /// archive, where its folder becomes a project. Bundles are left as they are.
    ImportBundles {
        #[arg(long)]
        root: String,
        /// Archive folder the projects go into.
        #[arg(long, default_value = "/")]
        to: String,
        /// Where to unpack (default: import-staging/ in the archive folder).
        #[arg(long)]
        staging: Option<PathBuf>,
        #[arg(required = true)]
        bundles: Vec<PathBuf>,
    },
    /// Import the parts the move2storage tool staged but never bundled, from
    /// its job folder (the one holding allowed-parts.json). Bundled parts are
    /// skipped; import their bundles with import-bundles.
    ImportStaged {
        #[arg(long)]
        root: String,
        #[arg(long, default_value = "/")]
        to: String,
        #[arg(long)]
        staging: Option<PathBuf>,
        job: PathBuf,
    },
    /// Rebuild a catalog from the packs alone (last-resort recovery).
    Rebuild {
        #[arg(long)]
        root: String,
        #[arg(long)]
        out: PathBuf,
    },
    /// Manage the hourly maintenance entry in this user's crontab.
    Cron {
        #[command(subcommand)]
        action: CronAction,
    },
}

#[derive(Subcommand)]
enum JobAction {
    /// Run a recorded job in this process (normally started in the background).
    Run {
        #[arg(long)]
        id: String,
    },
    /// Show all jobs and their progress.
    List,
    /// Ask a running job to stop.
    Cancel {
        #[arg(long)]
        id: String,
    },
}

#[derive(Subcommand)]
enum CronAction {
    /// Add (or update) the entry for an archive.
    Install {
        #[arg(long)]
        root: String,
    },
    /// Remove the entry for an archive.
    Remove {
        #[arg(long)]
        root: String,
    },
    /// Print the entry that `install` would write.
    Show {
        #[arg(long)]
        root: String,
    },
}

fn main() -> ExitCode {
    // Started by ssh to ask a sign-in question: pass it to the app and exit.
    if let Some(code) = archive_core::link::askpass_main() {
        return ExitCode::from(code);
    }
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("archive-helper: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Expand a leading `~` using $HOME.
fn expand(root: &str) -> Result<PathBuf> {
    let p = if root == "~" || root.starts_with("~/") {
        let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
        let rest = root.trim_start_matches('~').trim_start_matches('/');
        if rest.is_empty() { home } else { home.join(rest) }
    } else {
        PathBuf::from(root)
    };
    if !p.is_absolute() {
        bail!("the archive location must be an absolute path: {root}");
    }
    Ok(p)
}

fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Serve { root, restricted } => {
            let root = expand(&root)?;
            let exe = std::env::current_exe().ok();
            let kick_root = root.clone();
            let opts = ServeOptions {
                root,
                restricted,
                helper_version: format!("archive-helper {}", long_version()),
                kick_worker: Some(Box::new(move || {
                    if let Some(exe) = &exe {
                        spawn_detached_worker(exe, &kick_root);
                    }
                })),
            };
            serve::serve(io::stdin().lock(), io::stdout().lock(), opts)?;
        }
        Cmd::Files { start } => {
            let start = expand(&start)?;
            let exe = std::env::current_exe().ok();
            let opts = archive_core::fileserve::FilesOptions {
                start,
                helper_version: format!("archive-helper {}", long_version()),
                askpass: exe.clone().unwrap_or_else(|| PathBuf::from("archive-helper")),
                start_job: Box::new(move |id| {
                    if let Some(exe) = &exe {
                        let _ = archive_core::util::spawn_detached(exe, &["job", "run", "--id", id]);
                    }
                }),
            };
            archive_core::fileserve::serve_files(io::stdin().lock(), io::stdout().lock(), opts)?;
        }
        Cmd::Job { action } => match action {
            JobAction::Run { id } => archive_core::jobs::run(&id)?,
            JobAction::List => println!("{}", serde_json::to_string_pretty(&archive_core::jobs::list())?),
            JobAction::Cancel { id } => archive_core::jobs::cancel(&id)?,
        },
        Cmd::Worker { root, budget_minutes, force_daily, scrub_days } => {
            let root = expand(&root)?;
            let opts = WorkerOptions {
                budget: Duration::from_secs(budget_minutes * 60),
                force_daily,
                scrub_every_days: scrub_days,
                ..WorkerOptions::default()
            };
            match worker::run(&root, &opts)? {
                None => println!("another maintenance run is in progress"),
                Some(st) => println!("{}", serde_json::to_string_pretty(&st)?),
            }
        }
        Cmd::ImportBundles { root, to, staging, bundles } => {
            import_old(&root, &to, staging, |store, opts, on| archive_core::import::import(store, &bundles, opts, on))?
        }
        Cmd::ImportStaged { root, to, staging, job } => {
            import_old(&root, &to, staging, |store, opts, on| archive_core::import::import_staged(store, &job, opts, on))?
        }
        Cmd::Rebuild { root, out } => {
            let report = archive_core::rebuild::rebuild(&expand(&root)?, &out)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Cmd::Cron { action } => {
            let exe = std::env::current_exe()?;
            match action {
                CronAction::Show { root } => print!("{}", cron_block(&exe, &expand(&root)?)),
                CronAction::Install { root } => {
                    let root = expand(&root)?;
                    let current = read_crontab()?;
                    let updated = format!("{}{}", strip_block(&current, &root), cron_block(&exe, &root));
                    write_crontab(&updated)?;
                    println!("Installed hourly maintenance for {}", root.display());
                }
                CronAction::Remove { root } => {
                    let root = expand(&root)?;
                    let current = read_crontab()?;
                    write_crontab(&strip_block(&current, &root))?;
                    println!("Removed maintenance for {}", root.display());
                }
            }
        }
    }
    Ok(())
}

/// Start `archive-helper worker` in its own session so it outlives the SSH
/// connection. Its lock file makes extra starts harmless.
/// Run an import of the move2storage tool's data into the archive at `root`,
/// printing progress, then start maintenance for the new packs.
fn import_old(
    root: &str,
    to: &str,
    staging: Option<PathBuf>,
    run: impl FnOnce(
        &mut archive_core::store::Store,
        &archive_core::import::ImportOptions,
        &mut dyn FnMut(&archive_core::import::Event),
    ) -> archive_core::Result<Vec<archive_core::import::Receipt>>,
) -> Result<()> {
    use archive_core::import::{Event, ImportOptions};
    let root = expand(root)?;
    let mut store = archive_core::store::Store::open(&root)?;
    let opts = ImportOptions {
        dest: archive_core::VPath::parse(to)?,
        staging: staging.unwrap_or_else(|| root.join("import-staging")),
        receipts: root.join("imports"),
        send: Default::default(),
    };
    let mut last = std::time::Instant::now();
    let res = run(&mut store, &opts, &mut |e| match e {
        Event::Progress { done, total } => {
            if last.elapsed() >= Duration::from_secs(10) {
                last = std::time::Instant::now();
                println!("  {}%", done * 100 / (*total).max(1));
            }
        }
        Event::Checking { part } => println!("{part}: checking"),
        Event::Unpacking { part, files, bytes } => println!("{part}: unpacking {files} files ({})", archive_core::util::human_bytes(*bytes)),
        Event::Sending { part, project } => println!("{part}: archiving into {project}"),
        Event::AlreadyDone { part } => println!("{part}: already imported"),
        Event::Skipped { part, reason } => println!("{part}: skipped ({reason})"),
        Event::Imported { part, files } => println!("{part}: done, {files} files archived and verified"),
    });
    drop(store);
    // PAR2 and the rest for the new packs.
    spawn_detached_worker(&std::env::current_exe()?, &root);
    let receipts = res?;
    println!("{} parts imported. Receipts are in {}.", receipts.len(), opts.receipts.display());
    Ok(())
}

fn spawn_detached_worker(exe: &Path, root: &Path) {
    let root = root.display().to_string();
    let _ = archive_core::util::spawn_detached(exe, &["worker", "--root", &root]);
}

fn marker(root: &Path) -> (String, String) {
    (format!("# BEGIN archive-helper {}", root.display()), format!("# END archive-helper {}", root.display()))
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn cron_block(exe: &Path, root: &Path) -> String {
    let (begin, end) = marker(root);
    // Spread archives across the hour.
    let minute = root.to_string_lossy().bytes().fold(7u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32)) % 60;
    let cmd = format!("nice -n 19 {} worker --root {} >/dev/null 2>&1", quote(&exe.to_string_lossy()), quote(&root.to_string_lossy()));
    format!("{begin}\n{minute} * * * * {cmd}\n@reboot sleep 600 && {cmd}\n{end}\n")
}

fn strip_block(crontab: &str, root: &Path) -> String {
    let (begin, end) = marker(root);
    let mut out = String::new();
    let mut inside = false;
    for line in crontab.lines() {
        if line == begin {
            inside = true;
        } else if line == end {
            inside = false;
        } else if !inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn read_crontab() -> Result<String> {
    let out = Command::new("crontab").arg("-l").output().context("can't run crontab")?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        // "no crontab for user" is a normal empty state.
        Ok(String::new())
    }
}

fn write_crontab(content: &str) -> Result<()> {
    let mut child = Command::new("crontab").arg("-").stdin(Stdio::piped()).spawn().context("can't run crontab")?;
    child.stdin.take().unwrap().write_all(content.as_bytes())?;
    if !child.wait()?.success() {
        bail!("crontab rejected the new entries");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_block_round_trip() {
        let root = Path::new("/mnt/pool/me/archive");
        let block = cron_block(Path::new("/home/me/.local/bin/archive-helper"), root);
        assert!(block.contains("worker --root '/mnt/pool/me/archive'"));
        let existing = "MAILTO=me\n0 1 * * * backup\n";
        let installed = format!("{}{}", strip_block(existing, root), block);
        let reinstalled = format!("{}{}", strip_block(&installed, root), block);
        assert_eq!(installed, reinstalled, "install is idempotent");
        assert_eq!(strip_block(&installed, root), existing, "remove restores the original");
    }

    #[test]
    fn tilde_expansion() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand("~/archive").unwrap(), PathBuf::from(home).join("archive"));
        assert!(expand("relative/path").is_err());
    }
}
