//! Replacing the running `cx` with the latest release.
//!
//! The release archives are the ones `install.sh` fetches: one per platform on
//! the GitHub release, listed with their SHA-256 in `SHA256SUMS`. `cx upgrade`
//! reads that list from the latest release, downloads the archive for this
//! machine, checks it against the list, and puts the new binary where the
//! running one is. Nothing is changed unless the checksum matches.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where the releases are. `CX_RELEASES_URL` replaces it, for tests and
/// mirrors.
pub const RELEASES: &str = "https://github.com/protoworks-dev/claude-cx/releases";

/// What a look at the latest release found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Latest {
    /// The latest released version, such as `0.7.0`.
    pub version: String,
    /// Whether it is newer than the running cx.
    pub newer: bool,
    /// The SHA-256 the release lists for this machine's archive, in hex.
    pub sha256: String,
}

/// What [`upgrade`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The running cx is already the latest release, or newer.
    Current(String),
    /// The binary at the path was replaced with this version.
    Replaced(String, PathBuf),
}

/// The release target of this build, such as `aarch64-apple-darwin`. `None`
/// on a platform no release is built for.
pub fn target() -> Option<String> {
    let chip = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        _ => return None,
    };
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        _ => return None,
    };
    Some(format!("{chip}-{os}"))
}

/// Whether version `a` is newer than version `b`. Both are dotted numbers; a
/// part that is not a number counts as 0.
pub fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parts(a) > parts(b)
}

/// Find this target's archive in the text of a `SHA256SUMS` file. Returns
/// the version in its name and its checksum.
pub fn listed(sums: &str, target: &str) -> Option<(String, String)> {
    let suffix = format!("-{target}.tar.gz");
    sums.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        let (sum, name) = (words.next()?, words.next()?.trim_start_matches('*'));
        let version = name.strip_prefix("cx-v")?.strip_suffix(&suffix)?;
        let plain = !version.is_empty() && version.chars().all(|c| c.is_ascii_digit() || c == '.');
        (plain && sum.len() == 64 && sum.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| (version.to_string(), sum.to_lowercase()))
    })
}

fn releases() -> String {
    std::env::var("CX_RELEASES_URL")
        .unwrap_or_else(|_| RELEASES.into())
        .trim_end_matches('/')
        .to_string()
}

fn fetch(url: &str) -> Result<Vec<u8>> {
    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(120)))
        .user_agent(concat!("claude-cx/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();
    let mut reply = agent.get(url).call().with_context(|| format!("fetch {url}"))?;
    reply
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_vec()
        .with_context(|| format!("read {url}"))
}

/// Look at the latest release. One small network request.
pub fn latest() -> Result<Latest> {
    let target = target().context("no release is built for this platform. Build cx with cargo instead.")?;
    let sums = String::from_utf8_lossy(&fetch(&format!("{}/latest/download/SHA256SUMS", releases()))?).into_owned();
    let (version, sha256) =
        listed(&sums, &target).with_context(|| format!("the latest release has no archive for {target}"))?;
    Ok(Latest {
        newer: newer(&version, env!("CARGO_PKG_VERSION")),
        version,
        sha256,
    })
}

/// Replace the running binary with the latest release, when that is newer
/// (or when `force` is set). The archive must match the release's checksum.
/// The new file is written beside the old one and renamed over it, so a cx
/// that is running keeps its own copy until it exits.
pub fn upgrade(force: bool) -> Result<Outcome> {
    let found = latest()?;
    if !found.newer && !force {
        return Ok(Outcome::Current(found.version));
    }
    let target = target().context("no release is built for this platform")?;
    let name = format!("cx-v{}-{target}", found.version);
    let archive = fetch(&format!("{}/download/v{}/{name}.tar.gz", releases(), found.version))?;
    let got = hex(&Sha256::digest(&archive));
    if got != found.sha256 {
        bail!(
            "the download does not match the release's checksum (got {got}, want {}). Nothing was changed.",
            found.sha256
        );
    }
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("find the running cx")?;
    let dir = exe.parent().context("the running cx has no directory")?;
    let fresh = unpack(&archive, &name, dir).with_context(|| {
        format!(
            "{} could not be written. If cx came from a package manager, upgrade it there.",
            dir.display()
        )
    })?;
    if let Err(error) = std::fs::rename(&fresh, &exe) {
        let _ = std::fs::remove_file(&fresh);
        return Err(error).with_context(|| format!("replace {}", exe.display()));
    }
    Ok(Outcome::Replaced(found.version, exe))
}

/// Unpack `<name>/cx` from the archive into a new file in `dir`, mode 755.
fn unpack(archive: &[u8], name: &str, dir: &Path) -> Result<PathBuf> {
    let work = dir.join(format!(".cx-upgrade-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir(&work)?;
    let result = (|| {
        let packed = work.join("cx.tar.gz");
        std::fs::write(&packed, archive)?;
        let out = crate::util::run_bounded(
            std::process::Command::new("tar")
                .arg("-xzf")
                .arg(&packed)
                .arg("-C")
                .arg(&work)
                .arg(format!("{name}/cx")),
            None,
            Duration::from_secs(60),
        )?;
        if out.code != 0 {
            bail!("tar could not unpack the archive: {}", out.stderr.trim());
        }
        let fresh = dir.join(format!(".cx.new-{}", std::process::id()));
        std::fs::copy(work.join(name).join("cx"), &fresh)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(fresh)
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number_not_by_text() {
        assert!(newer("0.10.0", "0.9.3"));
        assert!(newer("v1.0.0", "0.99.0"));
        assert!(!newer("0.6.0", "0.6.0"));
        assert!(!newer("0.6.0", "0.6.1"));
    }

    #[test]
    fn the_checksum_list_gives_this_targets_version_and_sum() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums =
            format!("{a}  cx-v0.7.1-aarch64-apple-darwin.tar.gz\n{b}  cx-v0.7.1-x86_64-unknown-linux-gnu.tar.gz\n");
        assert_eq!(
            listed(&sums, "x86_64-unknown-linux-gnu"),
            Some(("0.7.1".into(), "b".repeat(64)))
        );
        assert_eq!(listed(&sums, "aarch64-unknown-linux-gnu"), None);
        // A name that is not a plain version, or a sum of the wrong length, is not trusted.
        assert_eq!(
            listed(
                &format!("{a}  cx-v../x-aarch64-apple-darwin.tar.gz"),
                "aarch64-apple-darwin"
            ),
            None
        );
        assert_eq!(
            listed("abc  cx-v0.7.1-aarch64-apple-darwin.tar.gz", "aarch64-apple-darwin"),
            None
        );
    }
}
