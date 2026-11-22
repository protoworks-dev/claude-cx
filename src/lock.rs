//! Directory locks compatible with the npm `proper-lockfile` package, which
//! Claude Code uses around its token refresh and its config writes.
//!
//! The lock is a directory: `mkdir` either creates it or fails. A holder
//! refreshes the directory's mtime while it holds the lock, and a waiter may
//! remove a lock whose mtime is older than the staleness limit.
//!
//! Claude Code's limits, as documented by claude-swap from the 2.1.218
//! bundle: the two credential locks go stale after 60s, the config lock after
//! 10s, and holders refresh every 5s.

use anyhow::{Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

/// How old Claude Code lets its two credential locks get before a waiter
/// may remove them: 60 seconds.
pub const CREDENTIAL_STALE: Duration = Duration::from_secs(60);
/// How old Claude Code lets its config lock get before a waiter may
/// remove it: 10 seconds.
pub const CONFIG_STALE: Duration = Duration::from_secs(10);
const TOUCH_EVERY: Duration = Duration::from_secs(3);
const TAKEOVER_STALE: Duration = Duration::from_secs(5);

/// A held directory lock. A background thread refreshes the directory's
/// mtime every 3 seconds so that waiters do not take the lock for stale.
/// Dropping the value stops the thread and removes the directory.
pub struct DirLock {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    toucher: Option<JoinHandle<()>>,
}

fn age(path: &Path) -> Option<Duration> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(SystemTime::now().duration_since(modified).unwrap_or_default())
}

fn touch(path: &Path) -> std::io::Result<()> {
    fs::File::open(path)?.set_modified(SystemTime::now())
}

/// Remove a stale lock. Two waiters that both saw the same dead holder must
/// not both remove: the second would delete the first waiter's fresh lock.
/// A sibling directory serializes the check and the removal.
fn remove_if_still_stale(path: &Path, stale: Duration) -> bool {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".takeover");
    let takeover = path.with_file_name(name);
    if fs::create_dir(&takeover).is_err() {
        if age(&takeover).is_some_and(|a| a > TAKEOVER_STALE) {
            let _ = fs::remove_dir(&takeover);
        }
        return false;
    }
    let removed = age(path).is_some_and(|a| a > stale) && fs::remove_dir(path).is_ok();
    let _ = fs::remove_dir(&takeover);
    removed
}

impl DirLock {
    /// Take the lock at `path`, creating missing parent directories. Waits
    /// while another holder has it, and removes a lock whose mtime is older
    /// than `stale`. Fails when the lock is still held after `timeout`, or
    /// when the directory cannot be created for another reason.
    pub fn acquire(path: &Path, stale: Duration, timeout: Duration) -> Result<DirLock> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let start = Instant::now();
        loop {
            match fs::create_dir(path) {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => bail!("cannot create lock {}: {e}", path.display()),
            }
            if start.elapsed() > timeout {
                bail!(
                    "lock {} is held by another process (waited {}s)",
                    path.display(),
                    timeout.as_secs()
                );
            }
            let pause = match age(path) {
                Some(a) if a > stale => {
                    if remove_if_still_stale(path, stale) {
                        continue;
                    }
                    Duration::from_millis(50)
                }
                Some(_) => Duration::from_millis(150 + fastrand::u64(..150)),
                None => Duration::from_millis(50),
            };
            let left = timeout.saturating_sub(start.elapsed());
            std::thread::sleep(pause.min(left.max(Duration::from_millis(1))));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let toucher = {
            let (stop, path) = (stop.clone(), path.to_path_buf());
            std::thread::spawn(move || {
                let mut last = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                    if last.elapsed() >= TOUCH_EVERY {
                        if touch(&path).is_err() && !path.exists() {
                            return;
                        }
                        last = Instant::now();
                    }
                }
            })
        };
        Ok(DirLock {
            path: path.to_path_buf(),
            stop,
            toucher: Some(toucher),
        })
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.toucher.take() {
            let _ = handle.join();
        }
        let _ = fs::remove_dir(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cx-lock-{}-{}", name, fastrand::u32(..)));
        fs::create_dir_all(&dir).unwrap();
        dir.join("the.lock")
    }

    #[test]
    fn second_holder_waits_then_times_out() {
        let path = tmp("held");
        let _first = DirLock::acquire(&path, CREDENTIAL_STALE, Duration::from_secs(1)).unwrap();
        let second = DirLock::acquire(&path, CREDENTIAL_STALE, Duration::from_millis(300));
        assert!(second.is_err());
    }

    #[test]
    fn released_lock_can_be_taken_again() {
        let path = tmp("release");
        drop(DirLock::acquire(&path, CREDENTIAL_STALE, Duration::from_secs(1)).unwrap());
        assert!(!path.exists());
        DirLock::acquire(&path, CREDENTIAL_STALE, Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn stale_lock_is_taken_over() {
        let path = tmp("stale");
        fs::create_dir(&path).unwrap();
        let old = SystemTime::now() - Duration::from_secs(120);
        fs::File::open(&path).unwrap().set_modified(old).unwrap();
        DirLock::acquire(&path, CREDENTIAL_STALE, Duration::from_secs(2)).unwrap();
    }
}
