//! A cx command as a macOS LaunchAgent: started at login, restarted if it
//! dies. Used for automatic switching and for the menu-bar item.

use crate::util;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

/// A cx command kept running by launchd.
pub struct Unit {
    /// The launchd label, which also names the plist in
    /// `~/Library/LaunchAgents`.
    pub label: &'static str,
    /// The cx subcommand it runs.
    pub argument: &'static str,
    /// Its log file under the cx state directory.
    pub log: &'static str,
    /// What to call it in messages.
    pub what: &'static str,
}

/// The LaunchAgent that keeps `cx auto` running.
pub const AUTO: Unit = Unit {
    label: "dev.cx.auto",
    argument: "auto",
    log: "auto.log",
    what: "Automatic switching",
};

fn plist_path(unit: &Unit) -> PathBuf {
    util::home()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", unit.label))
}

fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn launchctl(args: &[&str]) -> Result<util::Output> {
    util::run_bounded(Command::new("/bin/launchctl").args(args), None, Duration::from_secs(15))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Write the unit's plist to `~/Library/LaunchAgents` and load it, replacing
/// a loaded copy. The job runs the current executable, so the binary must
/// stay at its path. Prints the outcome to stdout. Fails off macOS and when
/// `launchctl bootstrap` refuses the job.
pub fn install(unit: &Unit) -> Result<()> {
    let label = unit.label;
    if !cfg!(target_os = "macos") {
        bail!(
            "the background service is macOS only. Elsewhere, run `cx auto` under your own supervisor, or `cx auto --once` from cron."
        );
    }
    let binary = std::env::current_exe().context("cannot find the cx binary")?;
    let log = util::store_root().join(unit.log);
    util::mkdir_private(&util::store_root())?;
    // launchd starts jobs with a bare PATH; cx needs `claude` only for
    // `cx run` and `cx login`, never for `cx auto`, so none is passed on.
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>{}</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        escape(&binary.to_string_lossy()),
        unit.argument,
        escape(&log.to_string_lossy()),
        escape(&log.to_string_lossy()),
    );
    let path = plist_path(unit);
    util::atomic_write(&path, plist.as_bytes(), 0o644)?;
    let _ = launchctl(&["bootout", &format!("{}/{label}", domain())]);
    let out = launchctl(&["bootstrap", &domain(), &path.to_string_lossy()])?;
    if out.code != 0 {
        bail!("launchctl could not start the service: {}", out.stderr.trim());
    }
    println!("{} now runs in the background and starts at login.", unit.what);
    println!("Log: {}", log.display());
    Ok(())
}

/// Stop the unit and delete its plist. Prints the outcome to stdout. A unit
/// that was never installed is not an error.
pub fn uninstall(unit: &Unit) -> Result<()> {
    let label = unit.label;
    let _ = launchctl(&["bootout", &format!("{}/{label}", domain())]);
    match std::fs::remove_file(plist_path(unit)) {
        Ok(()) => println!("{} is stopped and removed.", unit.what),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("{} was not installed.", unit.what),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Print to stdout whether the unit is installed and loaded, with the state
/// and pid `launchctl print` reports and the path of its log.
pub fn status(unit: &Unit) -> Result<()> {
    let label = unit.label;
    if !plist_path(unit).exists() {
        println!("{} is not installed to start at login.", unit.what);
        return Ok(());
    }
    let out = launchctl(&["print", &format!("{}/{label}", domain())])?;
    if out.code != 0 {
        println!("{} is installed but not loaded. Install it again.", unit.what);
        return Ok(());
    }
    let field = |name: &str| {
        out.stdout.lines().find_map(|l| {
            l.trim()
                .strip_prefix(name)
                .map(|v| v.trim_start_matches([' ', '=']).trim().to_string())
        })
    };
    println!(
        "{} is installed. State: {}. Pid: {}.",
        unit.what,
        field("state").unwrap_or_else(|| "unknown".into()),
        field("pid").unwrap_or_else(|| "none".into())
    );
    println!("Log: {}", util::store_root().join(unit.log).display());
    Ok(())
}
