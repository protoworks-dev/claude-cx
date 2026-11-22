//! Paths, atomic file writes, time helpers and bounded subprocess runs.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The user's home directory, from `$HOME`. Panics when `HOME` is not set.
pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME is not set"))
}

/// Where cx keeps its own state: `$CX_HOME`, else `~/.cx`.
pub fn store_root() -> PathBuf {
    match std::env::var_os("CX_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => home().join(".cx"),
    }
}

/// The current time in milliseconds since the Unix epoch. Returns 0 when
/// the system clock is set before the epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Create a directory and any missing parents, then set the directory
/// itself to mode 0700. Fails when it cannot be created. A failure to set
/// the mode is ignored, and parents keep the mode the umask gave them.
pub fn mkdir_private(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    Ok(())
}

/// Write a file through a temporary sibling and a rename, so a reader never
/// sees a half-written file. `mode` applies to a new file; an existing file
/// keeps its own mode.
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let dir = path.parent().context("path has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mode = fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(mode);
    let tmp = dir.join(format!(
        ".{}.cx-{}-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id(),
        fastrand::u32(..)
    ));
    let result = (|| -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Read a JSON file. `Ok(None)` means the file is missing, empty or only
/// whitespace. Fails when the file cannot be read or is not JSON.
pub fn read_json(path: &Path) -> Result<Option<Value>> {
    match fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(None),
        Ok(text) => Ok(Some(
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Write a value as indented JSON with a final newline, through
/// [`atomic_write`]. `mode` applies only when the file is new.
pub fn write_json(path: &Path, value: &Value, mode: u32) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    atomic_write(path, text.as_bytes(), mode)
}

/// What a helper process run by [`run_bounded`] left behind.
pub struct Output {
    /// The exit code, or -1 when the helper was ended by a signal.
    pub code: i32,
    /// Everything the helper wrote to standard output. Empty when the output
    /// was not valid UTF-8.
    pub stdout: String,
    /// Everything the helper wrote to standard error. Empty when the output
    /// was not valid UTF-8.
    pub stderr: String,
}

/// Run a helper to completion with a deadline. A helper that outlives the
/// deadline is killed and reported as an error.
pub fn run_bounded(cmd: &mut Command, stdin: Option<&str>, timeout: Duration) -> Result<Output> {
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawn helper")?;
    if let Some(text) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        let _ = pipe.write_all(text.as_bytes());
    }
    let mut out_pipe = child.stdout.take().context("no stdout")?;
    let mut err_pipe = child.stderr.take().context("no stderr")?;
    let out_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out_pipe.read_to_string(&mut s);
        s
    });
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err_pipe.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("helper timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(Output {
        code: status.code().unwrap_or(-1),
        stdout: out_thread.join().unwrap_or_default(),
        stderr: err_thread.join().unwrap_or_default(),
    })
}

/// Whether a process with this id exists, under any user. Always false
/// for pid 0.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // Signal 0 checks for existence. EPERM means it exists under another user.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// "4d 3h", "1h 12m", "9m", "now".
pub fn human_duration(secs: i64) -> String {
    if secs <= 0 {
        return "now".into();
    }
    let (d, h, m) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{secs}s")
    }
}

/// Milliseconds since the epoch for an RFC 3339 timestamp.
pub fn parse_rfc3339_ms(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.timestamp_millis())
}
