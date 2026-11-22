//! Secret storage: the macOS Keychain through `/usr/bin/security`, or
//! owner-only files when `CX_SECRETS=file` or the platform has no Keychain.
//!
//! The Keychain is driven through Apple's `security` tool and not through
//! Security.framework. An item's access list names the program that created
//! it, and `security` is the same binary on every run, so reads stay silent
//! after cx is rebuilt. Claude Code reads and writes its own items the same
//! way, which is why cx can read them without a prompt.

use crate::util::{self, run_bounded};
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const SECURITY: &str = "/usr/bin/security";
const NOT_FOUND: i32 = 44;
const TIMEOUT: Duration = Duration::from_secs(8);
/// `security -i` reads each command into a 4096-byte line buffer.
const STDIN_LINE_LIMIT: usize = 4096 - 64;

/// Where secrets are kept on this machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    /// The user's macOS login Keychain, through `/usr/bin/security`.
    Keychain,
    /// One file per secret under `secrets/` in the cx state directory,
    /// readable by the owner only (mode 0600) and not encrypted.
    File,
}

/// The backend in use: [`Backend::File`] when `CX_SECRETS=file` or the
/// platform is not macOS, otherwise [`Backend::Keychain`]. The environment
/// is read on every call.
pub fn backend() -> Backend {
    let forced = std::env::var("CX_SECRETS").unwrap_or_default();
    if forced == "file" || !cfg!(target_os = "macos") {
        Backend::File
    } else {
        Backend::Keychain
    }
}

/// The account name Claude Code uses for its Keychain items.
pub fn keychain_user() -> String {
    if let Ok(user) = std::env::var("USER")
        && !user.is_empty()
    {
        return user;
    }
    unsafe {
        let pw = libc::getpwuid(libc::geteuid());
        if !pw.is_null()
            && let Ok(name) = std::ffi::CStr::from_ptr((*pw).pw_name).to_str()
        {
            return name.to_string();
        }
    }
    "claude-code-user".into()
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Read the generic-password item for `service` and `account` from the
/// Keychain by running `security`. `Ok(None)` when there is no such item.
/// Fails when `security` exits with any other error or takes longer than 8
/// seconds, for example while a Keychain prompt waits for the user. The
/// returned value is secret and must not be logged.
pub fn keychain_get(service: &str, account: &str) -> Result<Option<String>> {
    let out = run_bounded(
        Command::new(SECURITY).args(["find-generic-password", "-a", account, "-w", "-s", service]),
        None,
        TIMEOUT,
    )?;
    match out.code {
        0 => Ok(Some(out.stdout.strip_suffix('\n').unwrap_or(&out.stdout).to_string())),
        NOT_FOUND => Ok(None),
        code => bail!(
            "Keychain read of '{service}' failed (exit {code}): {}",
            out.stderr.trim()
        ),
    }
}

/// Create the Keychain item for `service` and `account`, or replace its
/// value, by running `security`. The value is passed on stdin when the
/// command fits `security`'s line buffer. A longer one goes on the command
/// line, hex-encoded, where other processes of the same user can see it
/// while `security` runs. Fails on a non-zero exit or after 8 seconds.
pub fn keychain_set(service: &str, account: &str, value: &str) -> Result<()> {
    let hex: String = value.bytes().map(|b| format!("{b:02x}")).collect();
    // The value goes through stdin so it never shows in the process list. A
    // command longer than the line buffer would be cut mid-argument, so only
    // then does it go on the command line, still hex-encoded.
    let line = format!(
        "add-generic-password -U -a {} -s {} -X {}\n",
        quote(account),
        quote(service),
        hex
    );
    let out = if line.len() <= STDIN_LINE_LIMIT {
        run_bounded(Command::new(SECURITY).arg("-i"), Some(&line), TIMEOUT)?
    } else {
        run_bounded(
            Command::new(SECURITY).args(["add-generic-password", "-U", "-a", account, "-s", service, "-X", &hex]),
            None,
            TIMEOUT,
        )?
    };
    if out.code != 0 {
        bail!(
            "Keychain write of '{service}' failed (exit {}): {}",
            out.code,
            out.stderr.trim()
        );
    }
    Ok(())
}

/// Delete the Keychain item for `service` and `account` by running
/// `security`. Succeeds when the item was already gone.
pub fn keychain_delete(service: &str, account: &str) -> Result<()> {
    let out = run_bounded(
        Command::new(SECURITY).args(["delete-generic-password", "-a", account, "-s", service]),
        None,
        TIMEOUT,
    )?;
    match out.code {
        0 | NOT_FOUND => Ok(()),
        code => bail!(
            "Keychain delete of '{service}' failed (exit {code}): {}",
            out.stderr.trim()
        ),
    }
}

/// cx's own secrets, one per account, keyed by a short name such as `acct-3`.
pub const SERVICE: &str = "cx-accounts";

fn secret_file(name: &str) -> PathBuf {
    util::store_root().join("secrets").join(format!("{name}.secret"))
}

/// Read one of cx's own secrets from the current [`backend`]: the Keychain
/// item under [`SERVICE`], or the file `secrets/<name>.secret`. `Ok(None)`
/// when it was never stored. The returned value is secret and must not be
/// logged.
pub fn get(name: &str) -> Result<Option<String>> {
    match backend() {
        Backend::Keychain => keychain_get(SERVICE, name),
        Backend::File => match std::fs::read_to_string(secret_file(name)) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("read secret file"),
        },
    }
}

/// Store one of cx's own secrets in the current [`backend`], replacing any
/// earlier value. The file backend writes the file atomically with mode
/// 0600 and creates its directory as private.
pub fn set(name: &str, value: &str) -> Result<()> {
    match backend() {
        Backend::Keychain => keychain_set(SERVICE, name, value),
        Backend::File => {
            let path = secret_file(name);
            util::mkdir_private(path.parent().unwrap())?;
            util::atomic_write(&path, value.as_bytes(), 0o600)
        }
    }
}

/// Remove one of cx's own secrets from the current [`backend`]. Succeeds
/// when it was already gone.
pub fn delete(name: &str) -> Result<()> {
    match backend() {
        Backend::Keychain => keychain_delete(SERVICE, name),
        Backend::File => match std::fs::remove_file(secret_file(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("delete secret file"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes, reads and deletes a scratch item in the real login Keychain.
    /// Run by hand: `cargo test --bin cx keychain_round_trip -- --ignored`.
    #[test]
    #[ignore = "touches the real macOS Keychain"]
    fn keychain_round_trip() {
        let (service, user) = ("cx-selftest", keychain_user());
        let small = r#"{"claudeAiOauth":{"accessToken":"at","refreshToken":"rt"},"q":"a\"b\\c d"}"#.to_string();
        // Longer than the `security -i` line buffer once hex-encoded.
        let big = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{}"}},"mcpOAuth":"{}"}}"#,
            "a".repeat(900),
            "y".repeat(2500)
        );
        for value in [&small, &big, &small] {
            keychain_set(service, &user, value).unwrap();
            assert_eq!(keychain_get(service, &user).unwrap().as_deref(), Some(value.as_str()));
        }
        keychain_delete(service, &user).unwrap();
        assert_eq!(keychain_get(service, &user).unwrap(), None);
        keychain_delete(service, &user).unwrap();
    }
}
