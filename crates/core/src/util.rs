use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

pub use crate::catalog::now_secs;

pub fn now_ns() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

/// `YYYY-MM` for a Unix timestamp (UTC).
pub fn year_month(secs: i64) -> String {
    let (y, m, _) = civil(secs);
    format!("{y:04}-{m:02}")
}

/// `YYYY-MM-DD` for a Unix timestamp (UTC).
pub fn date(secs: i64) -> String {
    let (y, m, d) = civil(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

fn civil(secs: i64) -> (i64, i64, i64) {
    // Howard Hinnant's civil_from_days.
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let d = doy - (153 * mp + 2) / 5 + 1;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

/// Make a directory entry durable (so a newly created file survives a crash).
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Flush a file to the storage device. On macOS this is a plain `fsync`,
/// which is enough for downloads and far faster than `F_FULLFSYNC`.
pub fn fsync_light(f: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::fsync(f.as_raw_fd()) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        f.sync_data()
    }
}

/// Free space available to this user on the filesystem holding `path`.
pub fn free_space(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        Some(st.f_bavail as u64 * st.f_frsize as u64)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// The kind of network share holding `path` ("smbfs", "nfs", ...), or `None`
/// for a local disk (or if it can't be told).
pub fn network_fs(path: &Path) -> Option<String> {
    #[cfg(any(target_os = "macos", target_os = "freebsd"))]
    {
        use std::ffi::{CStr, CString};
        use std::os::unix::ffi::OsStrExt;
        let c = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        let name = unsafe { CStr::from_ptr(st.f_fstypename.as_ptr()) }.to_string_lossy().into_owned();
        ["smbfs", "nfs", "afpfs", "webdav", "cifs"].contains(&name.as_str()).then_some(name)
    }
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        match st.f_type as i64 {
            0x6969 => Some("nfs".into()),
            0x517B => Some("smbfs".into()),
            0xFF534D42 | 0xFE534D42 => Some("cifs".into()),
            _ => None,
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "freebsd", target_os = "linux")))]
    {
        let _ = path;
        None
    }
}

/// This machine's name.
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            return String::from_utf8_lossy(&buf[..end]).into_owned();
        }
    }
    std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_default()
}

/// Start a program in its own session, detached from this one's terminal and
/// connection, at low priority, so it keeps running after an SSH session ends.
pub fn spawn_detached(exe: &Path, args: &[&str]) -> io::Result<()> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::setpriority(libc::PRIO_PROCESS, 0, 10);
                Ok(())
            });
        }
    }
    cmd.spawn().map(|_| ())
}

/// Take an exclusive advisory lock on a file without waiting. Returns false if
/// another process holds it. The lock lasts until every descriptor sharing
/// this open file is closed.
pub fn try_lock(f: &File) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::WouldBlock { Ok(false) } else { Err(e) }
    }
    #[cfg(not(unix))]
    {
        let _ = f;
        Ok(true)
    }
}

/// Ask the OS to evict cached pages so a read-back really comes from disk.
/// Best effort: only effective on Linux.
#[allow(unused_variables)]
pub fn drop_cache(f: &File, off: u64, len: u64) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::posix_fadvise(f.as_raw_fd(), off as libc::off_t, len as libc::off_t, libc::POSIX_FADV_DONTNEED);
        }
    }
}

/// Write a file atomically: temp file, fsync, rename, fsync the directory.
pub fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".{}.tmp-{}", path.file_name().and_then(|n| n.to_str()).unwrap_or("file"), std::process::id()));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    sync_dir(dir)
}

pub fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    getrandom::getrandom(&mut b).expect("OS random source unavailable");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// "name (2).ext" style alternatives for keep-both conflicts.
pub fn numbered_name(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{} ({n}){}", &name[..i], &name[i..]),
        _ => format!("{name} ({n})"),
    }
}

/// Human-readable byte count ("1.9 TB").
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else if v < 10.0 {
        format!("{v:.1} {}", UNITS[i])
    } else {
        format!("{v:.0} {}", UNITS[i])
    }
}

/// Transfer speed over the last few seconds, from a running total of bytes.
/// Unlike an average since the start, it shows what's happening now.
pub struct Speed {
    window: std::time::Duration,
    samples: std::collections::VecDeque<(std::time::Instant, u64)>,
}

impl Speed {
    pub fn new(window: std::time::Duration) -> Speed {
        Speed { window, samples: std::collections::VecDeque::new() }
    }

    /// Record the running total now; returns bytes per second over the window.
    pub fn update(&mut self, done: u64) -> u64 {
        let now = std::time::Instant::now();
        if self.samples.back().is_some_and(|&(_, d)| done < d) {
            self.samples.clear(); // a file was sent again from the start
        }
        self.samples.push_back((now, done));
        while self.samples.len() > 2 && now.duration_since(self.samples[1].0) >= self.window {
            self.samples.pop_front();
        }
        let (t0, d0) = self.samples[0];
        let secs = now.duration_since(t0).as_secs_f64();
        if secs < 0.5 { 0 } else { ((done - d0) as f64 / secs) as u64 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_stamps() {
        assert_eq!(touch_stamp_utc(0), "197001010000.00");
        assert_eq!(touch_stamp_utc(951_782_400), "200002290000.00", "a leap day");
        assert_eq!(touch_stamp_utc(1_700_000_123), "202311142215.23");
        assert_eq!(touch_stamp_utc(-1), "196912312359.59");
    }

    #[test]
    fn dates_and_names() {
        assert_eq!(year_month(0), "1970-01");
        assert_eq!(year_month(1_790_000_000), "2026-09");
        assert_eq!(year_month(951_782_400), "2000-02"); // 2000-02-29
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(numbered_name("a.txt", 2), "a (2).txt");
        assert_eq!(numbered_name(".bashrc", 2), ".bashrc (2)");
        assert_eq!(numbered_name("dir", 3), "dir (3)");
        assert_eq!(human_bytes(1_900_000_000_000), "1.9 TB");
        assert_eq!(human_bytes(512), "512 B");
    }

    #[test]
    fn speed_follows_recent_progress() {
        let mut sp = Speed::new(std::time::Duration::from_millis(600));
        assert_eq!(sp.update(0), 0);
        std::thread::sleep(std::time::Duration::from_millis(700));
        let fast = sp.update(70_000_000);
        assert!((80_000_000..=110_000_000).contains(&fast), "{fast}");
        // Nothing new for a while: the speed falls toward zero.
        std::thread::sleep(std::time::Duration::from_millis(700));
        assert!(sp.update(70_000_000) < fast / 2);
    }
}

/// Append a line to this user's transfer timing log
/// (~/.local/share/archive-helper/timing.log), for looking at speed later.
pub fn log_timing(line: &str) {
    let Some(home) = std::env::var_os("HOME") else { return };
    let path = Path::new(&home).join(".local/share/archive-helper/timing.log");
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {line}", now_secs());
    }
}

/// A time (seconds since 1970, UTC) in `touch -t` form, CCYYMMDDhhmm.SS, for
/// recovery scripts that set file times with only standard tools.
pub fn touch_stamp_utc(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}{:02}{:02}.{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}
