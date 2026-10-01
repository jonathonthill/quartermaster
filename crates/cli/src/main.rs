//! `archive`: command-line access to an archive on local (or mounted) disk.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use archive_core::catalog::{self, NodeKind};
use archive_core::maint;
use archive_core::par2::Par2;
use archive_core::proto::InitOptions;
use archive_core::remote::{Remote, SshTarget};
use archive_core::retrieve::{self, LocalConflict, RetrieveOptions};
use archive_core::send::{self, Mode, SendOptions};
use archive_core::store::{Config, Store};
use archive_core::util::{date, human_bytes};
use archive_core::{Archive, Conflict, Entry, VPath};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "archive", version, about = "Store data in a compressed, checksummed, PAR2-protected archive")]
struct Cli {
    /// Archive folder, or ssh://[user@]host[:port]/path for one on a server
    /// (or set ARCHIVE_ROOT).
    #[arg(short, long, env = "ARCHIVE_ROOT", global = true)]
    archive: Option<String>,
    /// Print machine-readable JSON lines instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnConflict {
    Skip,
    Replace,
    KeepBoth,
    Fail,
}

/// Archived projects are frozen, so sending never replaces anything.
#[derive(Clone, Copy, ValueEnum)]
enum PutConflict {
    Skip,
    KeepBoth,
    Fail,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a new, empty archive.
    Init {
        #[arg(long, default_value_t = 32)]
        pack_size_gib: u64,
        #[arg(long, default_value_t = 10)]
        par2_percent: u32,
        #[arg(long, default_value_t = 30)]
        trash_days: u32,
    },
    /// Copy (or move) local files and folders into the archive. Each folder
    /// becomes a project, unless --to is inside a project (then its files are
    /// added to that project).
    Put {
        #[arg(required = true)]
        sources: Vec<PathBuf>,
        /// Destination folder in the archive.
        #[arg(long, default_value = "/")]
        to: String,
        /// Put everything in a new project folder of this name (to send loose files).
        #[arg(long)]
        new_project: Option<String>,
        /// Delete each local file after the archive has verified it.
        #[arg(long = "move")]
        move_: bool,
        /// When a different file is already at a destination in a project.
        #[arg(long, value_enum, default_value = "skip")]
        on_conflict: PutConflict,
        /// zstd level (1-19).
        #[arg(long, default_value_t = 9)]
        level: i32,
        /// Reuse a job key to resume an interrupted transfer.
        #[arg(long)]
        job: Option<String>,
        /// Extra name patterns to leave out (`*` and `?` wildcards).
        #[arg(long)]
        exclude: Vec<String>,
        /// Don't leave out OS files like .DS_Store and ._*.
        #[arg(long)]
        include_system_files: bool,
    },
    /// Copy a file or folder out of the archive.
    Get {
        path: String,
        #[arg(default_value = ".")]
        dest: PathBuf,
        #[arg(long, value_enum, default_value = "skip")]
        on_conflict: OnConflict,
    },
    /// List a folder.
    Ls {
        #[arg(default_value = "/")]
        path: String,
        #[arg(short, long)]
        long: bool,
    },
    /// Sizes, space used, duplicates, and protection status.
    Info {
        #[arg(default_value = "/")]
        path: String,
    },
    /// Search the archive. Every word must match a name or a folder above it
    /// (as a prefix). Filters: type:fastq, is:folder, is:file, after:2023-06,
    /// before:2024, >1GB, <10MB; "quoted words" must appear together.
    Find {
        query: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    Mkdir {
        path: String,
    },
    /// Rename or move within the archive.
    Mv {
        from: String,
        to: String,
    },
    /// Move items to the trash.
    Rm {
        #[arg(required = true)]
        paths: Vec<String>,
    },
    #[command(subcommand)]
    Trash(TrashCmd),
    /// Seal open packs (e.g. from interrupted transfers).
    Seal {
        /// Only packs idle for at least this many minutes.
        #[arg(long)]
        idle_minutes: Option<i64>,
    },
    /// Create PAR2 recovery data for sealed packs.
    Protect {
        #[arg(long)]
        par2: Option<PathBuf>,
    },
    /// Verify all packs, repairing damage from recovery data.
    Scrub {
        #[arg(long)]
        par2: Option<PathBuf>,
    },
    /// List packs and their state.
    Packs,
    /// Start a maintenance pass on the server (PAR2, checks, purge) in the background.
    Maintain,
    /// Show the server's last maintenance report.
    Status,
    /// Write INDEX.tsv, the plain-text list of every file and the pack holding
    /// it (servers write it daily during maintenance).
    Index,
    #[command(subcommand)]
    Helper(HelperCmd),
}

#[derive(Subcommand)]
enum HelperCmd {
    /// Upload archive-helper to a server's ~/.local/bin, verifying its checksum.
    Install {
        /// SSH host (an alias from ~/.ssh/config, or user@host).
        #[arg(long)]
        host: String,
        /// Helper binary to upload; by default, picked from --helpers by the server's OS and CPU.
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Folder of helper builds named archive-helper-<os>-<arch>.
        #[arg(long, env = "ARCHIVE_HELPERS", default_value = "dist/helpers")]
        helpers: PathBuf,
    },
}

#[derive(Subcommand)]
enum TrashCmd {
    /// Show what's in the trash.
    List,
    /// Put an item back (by the id shown in `trash list`).
    Restore {
        id: i64,
        #[arg(long)]
        to: Option<String>,
    },
    /// Permanently delete old trash and free the space it used.
    Purge {
        /// Override the archive's trash period.
        #[arg(long)]
        older_than_days: Option<u32>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn vpath(s: &str) -> Result<VPath> {
    VPath::parse(s).with_context(|| format!("bad archive path {s:?}"))
}

/// Throttled single-line progress on stderr.
struct Progress {
    tty: bool,
    last: Instant,
    speed: archive_core::util::Speed,
}

impl Progress {
    fn new() -> Self {
        let speed = archive_core::util::Speed::new(Duration::from_secs(10));
        Progress { tty: std::io::stderr().is_terminal(), last: Instant::now() - Duration::from_secs(1), speed }
    }
    fn update(&mut self, done: u64, total: u64, what: &str) {
        if !self.tty || self.last.elapsed() < Duration::from_millis(200) {
            return;
        }
        self.last = Instant::now();
        let pct = if total == 0 { 100 } else { done * 100 / total };
        let rate = self.speed.update(done);
        let name: String = what.chars().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect();
        eprint!("\r\x1b[2K  {pct:>3}%  {} of {}  {}/s  {name}", human_bytes(done), human_bytes(total), human_bytes(rate));
        let _ = std::io::stderr().flush();
    }
    fn clear(&self) {
        if self.tty {
            eprint!("\r\x1b[2K");
        }
    }
}

/// An archive on this machine or on a server.
enum Conn {
    Local(Box<Store>),
    Remote(Box<Remote>),
}

impl Conn {
    fn open(spec: &str) -> Result<Conn> {
        match SshTarget::parse(spec) {
            Some(t) => {
                let r = Remote::ssh(&t)?;
                if r.hello().archive_id.is_none() {
                    anyhow::bail!("no archive at {} on {} yet; create one with `archive init`", t.root, t.host);
                }
                Ok(Conn::Remote(Box::new(r)))
            }
            None => Ok(Conn::Local(Box::new(Store::open(&PathBuf::from(spec))?))),
        }
    }

    fn archive(&mut self) -> &mut dyn Archive {
        match self {
            Conn::Local(s) => s.as_mut(),
            Conn::Remote(r) => r.as_mut(),
        }
    }

    fn local(&mut self, what: &str) -> Result<&mut Store> {
        match self {
            Conn::Local(s) => Ok(s),
            Conn::Remote(_) => anyhow::bail!("`{what}` runs on the server itself; use `archive maintain` to start maintenance there"),
        }
    }
}

fn run(cli: Cli) -> Result<bool> {
    let spec = || -> Result<String> {
        cli.archive.clone().context("no archive given: use --archive PATH (or ssh://host/path) or set ARCHIVE_ROOT")
    };
    let connect = || -> Result<Conn> { Conn::open(&spec()?) };
    let json = cli.json;
    let out = |v: serde_json::Value| println!("{v}");

    match cli.cmd {
        Cmd::Init { pack_size_gib, par2_percent, trash_days } => {
            let spec = spec()?;
            if let Some(t) = SshTarget::parse(&spec) {
                let mut r = Remote::ssh(&t)?;
                r.init(InitOptions {
                    pack_target_bytes: Some(pack_size_gib << 30),
                    par2_redundancy_percent: Some(par2_percent),
                    trash_days: Some(trash_days),
                })?;
                println!("Created archive {} at {} on {}", r.hello().archive_id.clone().unwrap_or_default(), t.root, t.host);
            } else {
                let cfg = Config {
                    pack_target_bytes: pack_size_gib << 30,
                    par2_redundancy_percent: par2_percent,
                    trash_days,
                    ..Config::default()
                };
                let store = Store::init(&PathBuf::from(spec), cfg)?;
                if json {
                    out(serde_json::to_value(store.config())?);
                } else {
                    println!("Created archive {} at {}", store.config().archive_id, store.root().display());
                }
            }
        }
        Cmd::Put { sources, to, new_project, move_, on_conflict, level, job, exclude, include_system_files } => {
            let mut conn = connect()?;
            let policy = match on_conflict {
                PutConflict::Skip => Conflict::Skip,
                PutConflict::KeepBoth => Conflict::KeepBoth,
                PutConflict::Fail => Conflict::Fail,
            };
            let mut opts = SendOptions { mode: if move_ { Mode::Move } else { Mode::Copy }, policy, new_project, ..SendOptions::default() };
            opts.compress.level = level;
            if let Some(j) = job {
                opts.job = j;
            }
            if include_system_files {
                opts.exclude.clear();
            }
            opts.exclude.extend(exclude);
            let mut prog = Progress::new();
            let mut current = String::new();
            let report = send::send(conn.archive(), &sources, &vpath(&to)?, &opts, &mut |e| {
                if json {
                    println!("{}", serde_json::to_string(e).unwrap_or_default());
                    return;
                }
                match e {
                    send::Event::Scanned { files, bytes, folders } => {
                        eprintln!("Sending {files} files in {folders} folders ({}), job {}", human_bytes(*bytes), opts.job);
                    }
                    send::Event::Sending { path } | send::Event::Checking { path } => current = path.display().to_string(),
                    send::Event::Progress { done, total } => prog.update(*done, *total, &current),
                    send::Event::Failed { path, error } => {
                        prog.clear();
                        eprintln!("  failed: {}: {error}", path.display());
                    }
                    send::Event::Kept { path, reason } => {
                        prog.clear();
                        eprintln!("  kept: {}: {reason}", path.display());
                    }
                    send::Event::Sealing => prog.clear(),
                    _ => {}
                }
            })?;
            if json {
                out(serde_json::json!({ "event": "report", "report": report }));
            } else {
                println!(
                    "{} files ({}): {} stored, {} already stored elsewhere, {} already there, {} skipped, {} failed. Sent {}.",
                    report.files,
                    human_bytes(report.bytes),
                    report.stored,
                    report.deduplicated,
                    report.identical,
                    report.skipped.len(),
                    report.failed.len(),
                    human_bytes(report.bytes_sent)
                );
                if move_ {
                    println!("Deleted {} local files after verification; kept {}.", report.deleted, report.kept.len());
                }
                for (p, d) in &report.skipped {
                    println!("  skipped (different item already at {d}): {}", p.display());
                }
            }
            return Ok(report.ok());
        }
        Cmd::Get { path, dest, on_conflict } => {
            let mut conn = connect()?;
            let opts = RetrieveOptions {
                policy: match on_conflict {
                    OnConflict::Replace => LocalConflict::Replace,
                    OnConflict::KeepBoth => LocalConflict::KeepBoth,
                    _ => LocalConflict::Skip,
                },
                ..RetrieveOptions::default()
            };
            let mut prog = Progress::new();
            let mut current = String::new();
            let report = retrieve::retrieve(conn.archive(), &vpath(&path)?, &dest, &opts, &mut |e| {
                if json {
                    println!("{}", serde_json::to_string(e).unwrap_or_default());
                    return;
                }
                match e {
                    retrieve::Event::Receiving { path } => current = path.display().to_string(),
                    retrieve::Event::Progress { done, total } => prog.update(*done, *total, &current),
                    retrieve::Event::Failed { path, error } => {
                        prog.clear();
                        eprintln!("  failed: {}: {error}", path.display());
                    }
                    _ => {}
                }
            })?;
            prog.clear();
            if json {
                out(serde_json::json!({ "event": "report", "report": report }));
            } else {
                println!(
                    "{} files ({}): {} written and verified, {} skipped (already exist), {} failed.",
                    report.files,
                    human_bytes(report.bytes),
                    report.written,
                    report.skipped.len(),
                    report.failed.len()
                );
            }
            return Ok(report.ok());
        }
        Cmd::Ls { path, long } => {
            let entries = connect()?.archive().list(&vpath(&path)?)?;
            if json {
                out(serde_json::to_value(&entries)?);
            } else {
                for e in &entries {
                    print_entry(e, long);
                }
            }
        }
        Cmd::Info { path } => {
            let info = connect()?.archive().info(&vpath(&path)?)?;
            if json {
                out(serde_json::to_value(&info)?);
            } else {
                println!("{}", info.path);
                println!("  Contents       {} files, {} folders", info.files, info.folders);
                println!("  Original size  {}", human_bytes(info.original_bytes));
                let saved = if info.original_bytes > 0 { 100 - info.stored_bytes * 100 / info.original_bytes } else { 0 };
                println!("  Space used     {} ({saved}% saved)", human_bytes(info.stored_bytes));
                println!("  Duplicates     {} stored once", human_bytes(info.duplicate_bytes));
                if info.archived_at > 0 {
                    println!("  Archived       {}", date(info.archived_at));
                }
                let status = match info.protection {
                    maint::Protection::Protected => "Protected with recovery data",
                    maint::Protection::Pending => "Stored and verified; recovery data pending",
                    maint::Protection::Damaged => "DAMAGED: some data could not be repaired",
                    maint::Protection::None => "Nothing stored",
                };
                println!("  Status         {status}");
                if let Some(t) = info.last_checked {
                    println!("  Last checked   {}", date(t));
                }
            }
        }
        Cmd::Find { query, limit } => {
            let hits = connect()?.archive().search(&query, limit)?;
            if json {
                out(serde_json::to_value(hits.iter().map(|(p, e)| serde_json::json!({"path": p, "entry": e})).collect::<Vec<_>>())?);
            } else {
                for (p, e) in hits {
                    println!("{:>10}  {p}{}", human_bytes(e.size), if e.kind == NodeKind::Dir { "/" } else { "" });
                }
            }
        }
        Cmd::Mkdir { path } => connect()?.archive().create_folder(&vpath(&path)?)?,
        Cmd::Mv { from, to } => connect()?.archive().rename(&vpath(&from)?, &vpath(&to)?)?,
        Cmd::Rm { paths } => {
            let mut conn = connect()?;
            for p in paths {
                let id = conn.archive().trash(&vpath(&p)?)?;
                if !json {
                    println!("Moved {p} to the trash (id {id}). Restore with: archive trash restore {id}");
                }
            }
        }
        Cmd::Trash(t) => {
            let mut conn = connect()?;
            match t {
                TrashCmd::List => {
                    let items = conn.archive().trash_list()?;
                    if json {
                        out(serde_json::to_value(&items)?);
                    }
                    for e in items {
                        if !json {
                            println!(
                                "{:>6}  {:>10}  trashed {}  {}",
                                e.id,
                                human_bytes(e.size),
                                date(e.trashed_at.unwrap_or(0)),
                                e.trash_origin.as_deref().unwrap_or("?")
                            );
                        }
                    }
                }
                TrashCmd::Restore { id, to } => {
                    let to = to.map(|t| vpath(&t)).transpose()?;
                    let p = conn.archive().restore(id, to.as_ref())?;
                    println!("Restored to {p}");
                }
                TrashCmd::Purge { older_than_days } => {
                    let r = maint::purge(conn.local("trash purge")?, older_than_days)?;
                    if json {
                        out(serde_json::to_value(&r)?);
                    } else {
                        println!(
                            "Purged {} items; deleted {} packs, freeing {}.",
                            r.purged_items,
                            r.packs_deleted.len(),
                            human_bytes(r.bytes_freed)
                        );
                    }
                }
            }
        }
        Cmd::Seal { idle_minutes } => {
            let mut conn = connect()?;
            let n = conn.local("seal")?.seal_all_open(idle_minutes.map(|m| m * 60))?;
            println!("Sealed {n} packs.");
        }
        Cmd::Protect { par2 } => {
            let mut conn = connect()?;
            let store = conn.local("protect")?;
            let p = Par2::find(par2.as_deref())?;
            let res = maint::protect(store, &p, None, &mut |r| {
                if json {
                    println!("{}", serde_json::to_string(r).unwrap_or_default());
                } else {
                    println!("{}: {}", r.pack, r.outcome);
                }
            })?;
            return Ok(res.iter().all(|r| r.outcome == "protected"));
        }
        Cmd::Scrub { par2 } => {
            let mut conn = connect()?;
            let store = conn.local("scrub")?;
            let p = Par2::find(par2.as_deref())?;
            let res = maint::scrub(store, &p, maint::ScrubOptions::default(), &mut |r| {
                if json {
                    println!("{}", serde_json::to_string(r).unwrap_or_default());
                } else {
                    println!("{}: {}", r.pack, r.outcome);
                }
            })?;
            let bad = res.iter().filter(|r| r.outcome.starts_with("DAMAGED")).count();
            if bad > 0 && !json {
                eprintln!("{bad} packs are damaged beyond repair.");
            }
            return Ok(bad == 0);
        }
        Cmd::Packs => {
            let packs = match connect()? {
                Conn::Local(store) => catalog::packs(store.conn())?,
                Conn::Remote(mut r) => r.packs()?,
            };
            if json {
                out(serde_json::to_value(&packs)?);
            } else {
                for p in packs {
                    println!(
                        "{:<32} {:<10} {:>10}  {}",
                        p.name,
                        p.state.as_str(),
                        human_bytes(p.bytes),
                        p.check_result.as_deref().map(|r| format!("checked {}: {r}", date(p.checked_at.unwrap_or(0)))).unwrap_or_default()
                    );
                }
            }
        }
        Cmd::Maintain => match connect()? {
            Conn::Remote(mut r) => {
                r.kick_worker()?;
                println!("Maintenance started on the server; check progress with `archive status`.");
            }
            Conn::Local(_) => anyhow::bail!("for a local archive, run `archive protect` and `archive scrub`"),
        },
        Cmd::Index => match connect()? {
            Conn::Remote(mut r) => {
                r.kick_worker()?;
                println!("The server rewrites INDEX.tsv once a day during maintenance, which has been started.");
            }
            Conn::Local(store) => {
                let p = archive_core::worker::write_index(&store)?;
                println!("Wrote {}", p.display());
            }
        },
        Cmd::Status => {
            let status = match connect()? {
                Conn::Remote(mut r) => r.worker_status()?,
                Conn::Local(store) => serde_json::to_value(archive_core::worker::read_status(store.root()))?,
            };
            if json || status.is_null() {
                if status.is_null() && !json {
                    println!("No maintenance has run yet.");
                } else {
                    out(status);
                }
            } else {
                print_status(&status);
            }
        }
        Cmd::Helper(HelperCmd::Install { host, binary, helpers }) => {
            install_helper(&host, binary, &helpers)?;
        }
    }
    Ok(true)
}

fn print_status(s: &serde_json::Value) {
    let n = |k: &str| s.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let when = s.get("finished").and_then(|v| v.as_i64()).map(date).unwrap_or_default();
    println!("Last maintenance: {when}");
    println!("  Packs: {} total, {} protected, {} waiting for recovery data", n("packs_total"), n("packs_protected"), n("packs_pending"));
    if let Some(d) = s.get("packs_damaged").and_then(|v| v.as_array()).filter(|a| !a.is_empty()) {
        println!("  DAMAGED: {}", d.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "));
    }
    let count = |k: &str| s.get(k).and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    println!("  This run: protected {}, checked {}, sealed {}", count("protected"), count("scrubbed"), n("sealed"));
    if let Some(p) = s.get("snapshot").and_then(|v| v.as_str()) {
        println!("  Catalog snapshot: {p}");
    }
    for e in s.get("errors").and_then(|v| v.as_array()).into_iter().flatten() {
        println!("  problem: {}", e.as_str().unwrap_or(""));
    }
}

fn install_helper(host: &str, binary: Option<PathBuf>, helpers: &std::path::Path) -> Result<()> {
    let setup = archive_core::remote::SshSetup::default();
    let dir;
    let helpers = match binary {
        // A specific binary: stage it under the expected name.
        Some(b) => {
            let uname = archive_core::remote::run_ssh(host, None, &setup, "uname -sm", None)?;
            let (os, arch) = uname.trim().split_once(' ').context("unexpected uname output")?;
            let arch = if arch == "amd64" { "x86_64".to_string() } else { arch.replace("arm64", "aarch64") };
            dir = std::env::temp_dir().join(format!("archive-helper-stage-{}", std::process::id()));
            std::fs::create_dir_all(&dir)?;
            std::fs::copy(&b, dir.join(format!("archive-helper-{}-{arch}", os.to_ascii_lowercase())))?;
            dir.as_path()
        }
        None => helpers,
    };
    let version = archive_core::remote::install_helper(host, None, &setup, helpers)?;
    println!("Installed on {host}: {version}");
    Ok(())
}

fn print_entry(e: &Entry, long: bool) {
    let name = match e.kind {
        NodeKind::Dir => format!("{}/", e.name),
        NodeKind::Symlink => format!("{} -> {}", e.name, e.target.as_deref().unwrap_or("")),
        NodeKind::File => e.name.clone(),
    };
    if long {
        let when = date(e.mtime_ns.div_euclid(1_000_000_000));
        let count = if e.kind == NodeKind::Dir { format!("{} files", e.files) } else { String::new() };
        let project = if e.is_project { "  (project)" } else { "" };
        println!("{:>10}  {when}  {count:>12}  {name}{project}", human_bytes(e.size));
    } else {
        println!("{name}");
    }
}
