//! A file server signs in to an archive server and runs a job over the
//! signed-in link: the real helper, with a stand-in for ssh in which a "link"
//! is a marker file at the control socket's path.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use archive_core::jobs::{ArchiveTarget, JobSpec, JobStatus, SshAuth};
use archive_core::link::LinkTarget;
use archive_core::remote::Remote;
use archive_core::store::{Config, Store};
use archive_core::{Archive, VPath};

const FAKE_SSH: &str = r#"#!/bin/sh
cp=""; op=""; bg=0
while [ $# -gt 0 ]; do
  case "$1" in
    -o) case "$2" in ControlPath=*) cp="${2#ControlPath=}";; esac; shift 2;;
    -O) op="$2"; shift 2;;
    -F|-p|-l|-i) shift 2;;
    -f) bg=1; shift;;
    -*) shift;;
    *) break;;
  esac
done
host="$1"; shift
case "$op" in
  check) [ -e "$cp" ] && exit 0; exit 255;;
  exit) rm -f "$cp"; exit 0;;
esac
if [ "$bg" = 1 ]; then
  answer=$("$SSH_ASKPASS" "$host's password: ") || { echo "Permission denied (password)." >&2; exit 255; }
  [ "$answer" = "secret" ] || { echo "Permission denied (password)." >&2; exit 255; }
  : > "$cp"; exit 0
fi
[ -e "$cp" ] || { echo "Permission denied (publickey,password)." >&2; exit 255; }
exec sh -c "$*"
"#;

struct Session {
    remote: Option<Remote>,
    child: Child,
}

impl Drop for Session {
    fn drop(&mut self) {
        drop(self.remote.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn files_session(home: &Path, fake_ssh: &Path, start: &Path) -> Session {
    let mut child = Command::new(env!("CARGO_BIN_EXE_archive-helper"))
        .args(["files", "--start"])
        .arg(start)
        .env("HOME", home)
        .env("ARCHIVE_HELPER_SSH", fake_ssh)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let (r, w) = (child.stdout.take().unwrap(), child.stdin.take().unwrap());
    Session { remote: Some(Remote::connect(Box::new(r), Box::new(w)).unwrap()), child }
}

fn wait_for(remote: &mut Remote, id: &str, what: impl Fn(&JobStatus) -> bool) -> JobStatus {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let st = remote.job_list(false).unwrap().into_iter().find(|s| s.id == id).unwrap();
        if what(&st) {
            return st;
        }
        assert!(Instant::now() < deadline, "timed out; last status {st:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn sign_in_then_job_waits_and_resumes() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    let bin = home.join(".local/bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_archive-helper"), bin.join("archive-helper")).unwrap();
    let fake = t.path().join("fake-ssh");
    fs::write(&fake, FAKE_SSH).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let data = t.path().join("data/run7");
    fs::create_dir_all(&data).unwrap();
    for i in 0..4 {
        fs::write(data.join(format!("part{i}.txt")), format!("sample {i}\n").repeat(3000)).unwrap();
    }
    let root = t.path().join("archive");
    Store::init(&root, Config::default()).unwrap();

    let mut s = files_session(&home, &fake, t.path());
    let remote = s.remote.as_mut().unwrap();
    let host = format!("archive-{}.test", archive_core::util::random_hex(4));
    let target = LinkTarget { host: host.clone(), port: None, user: Some("me".into()) };
    assert!(!remote.link_check(&target).unwrap());

    // A wrong password and a cancelled prompt both fail, and say which.
    let mut asked = Vec::new();
    let e = remote
        .sign_in(&target, &mut |q| {
            asked.push(q.to_string());
            Some("wrong".into())
        })
        .unwrap_err();
    assert!(e.to_string().contains("didn't accept the sign-in"), "{e}");
    assert_eq!(asked, vec![format!("{host}'s password:")]);
    let e = remote.sign_in(&target, &mut |_| None).unwrap_err();
    assert!(e.to_string().contains("cancelled"), "{e}");
    assert!(!remote.link_check(&target).unwrap());

    remote.sign_in(&target, &mut |_| Some("secret".into())).unwrap();
    assert!(remote.link_check(&target).unwrap());
    // Signing in again while signed in asks nothing.
    remote.sign_in(&target, &mut |q| panic!("asked {q}")).unwrap();

    // Signed out, a job waits for the user to sign in again, then finishes.
    remote.sign_out(&target).unwrap();
    let spec = JobSpec {
        title: "run7 test".into(),
        archive_name: "Test archive".into(),
        direction: "send".into(),
        archive: ArchiveTarget::Ssh {
            host: host.clone(),
            port: None,
            user: Some("me".into()),
            root: root.display().to_string(),
            auth: SshAuth::SignIn,
        },
        sources: vec![data.display().to_string()],
        dest: "/Projects".into(),
        mode: "move".into(),
        conflict: "skip".into(),
        new_project: None,
    };
    let id = remote.job_start(&spec).unwrap();
    let st = wait_for(remote, &id, |s| s.state == "waiting");
    assert_eq!(st.link.as_ref(), Some(&target));
    assert_eq!(st.archive, "Test archive");
    assert!(data.join("part0.txt").exists(), "nothing moved while signed out");

    remote.sign_in(&target, &mut |_| Some("secret".into())).unwrap();
    let st = wait_for(remote, &id, |s| !matches!(s.state.as_str(), "waiting" | "running" | "queued"));
    assert_eq!(st.state, "done", "{st:?}");
    assert!(st.message.starts_with("4 of 4 files archived"), "{}", st.message);
    assert!(!data.exists(), "Move removed the sources");
    let mut store = Store::open(&root).unwrap();
    let e = store.stat(&VPath::parse("/Projects/run7").unwrap()).unwrap().unwrap();
    assert_eq!(e.files, 4);

    remote.sign_out(&target).unwrap();
    assert!(!remote.link_check(&target).unwrap());
}

/// Join `archive-helper job run` to `archive-helper serve`, as the app does over ssh.
fn relay(home: &Path, id: &str, serve: &mut Command) -> std::process::ExitStatus {
    let mut job = Command::new(env!("CARGO_BIN_EXE_archive-helper"))
        .args(["job", "run", "--id", id])
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut arc = serve.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let (mut jo, mut ji) = (job.stdout.take().unwrap(), job.stdin.take().unwrap());
    let (mut ao, mut ai) = (arc.stdout.take().unwrap(), arc.stdin.take().unwrap());
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut jo, &mut ai);
    });
    let down = std::thread::spawn(move || {
        let _ = std::io::copy(&mut ao, &mut ji);
    });
    let status = job.wait().unwrap();
    up.join().unwrap();
    let _ = arc.wait();
    down.join().unwrap();
    status
}

#[test]
fn relayed_job_runs_only_when_relayed() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let data = t.path().join("data/run9");
    fs::create_dir_all(data.join("sub")).unwrap();
    for i in 0..3 {
        fs::write(data.join(format!("sub/r{i}.txt")), format!("relay {i}\n").repeat(2000)).unwrap();
    }
    let root = t.path().join("archive");
    Store::init(&root, Config::default()).unwrap();

    let mut s = files_session(&home, Path::new("ssh"), t.path());
    let remote = s.remote.as_mut().unwrap();
    let spec = JobSpec {
        title: "run9 relayed".into(),
        archive_name: "Test archive".into(),
        direction: "send".into(),
        archive: ArchiveTarget::Relay { archive: "arc1".into(), root: root.display().to_string() },
        sources: vec![data.display().to_string()],
        dest: "/".into(),
        mode: "copy".into(),
        conflict: "skip".into(),
        new_project: None,
    };
    let id = remote.job_start(&spec).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let st = remote.job_list(true).unwrap().into_iter().find(|s| s.id == id).unwrap();
    assert_eq!(st.state, "queued", "the server doesn't start a relayed job by itself");
    assert_eq!(st.relay.as_deref(), Some("arc1"));

    // The relaying computer drops out: the job pauses, and the server leaves it for the relay to restart.
    let status = relay(&home, &id, &mut Command::new("true"));
    assert!(status.success());
    let st = remote.job_list(true).unwrap().into_iter().find(|s| s.id == id).unwrap();
    assert_eq!(st.state, "interrupted", "{st:?}");
    assert!(st.message.contains("goes through the computer that started it"), "{}", st.message);

    let mut serve = Command::new(env!("CARGO_BIN_EXE_archive-helper"));
    serve.args(["serve", "--root"]).arg(&root);
    assert!(relay(&home, &id, &mut serve).success());
    let st = remote.job_list(false).unwrap().into_iter().find(|s| s.id == id).unwrap();
    assert_eq!(st.state, "done", "{st:?}");
    assert!(st.message.starts_with("3 of 3 files archived"), "{}", st.message);
    let mut store = Store::open(&root).unwrap();
    assert_eq!(store.stat(&VPath::parse("/run9").unwrap()).unwrap().unwrap().files, 3);
}
