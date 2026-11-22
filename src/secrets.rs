//! Secret storage: the macOS Keychain through `/usr/bin/security`, or
//! owner-only files when `CX_SECRETS=file` or the platform has no Keychain.
//! A value never goes on a command line: one too long for `security`'s
//! stdin is kept in an owner-only file instead.
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

/// What [`keychain_set`] did with a value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Written {
    /// The item now holds the value.
    Stored,
    /// The value is too long to hand to `security` on stdin. Nothing was
    /// written; the caller must keep it somewhere else.
    TooLarge,
}

/// The one line `security -i` is given to store a value, or `None` when the
/// line would not fit its buffer.
fn store_line(service: &str, account: &str, value: &str) -> Option<String> {
    let hex: String = value.bytes().map(|b| format!("{b:02x}")).collect();
    let line = format!(
        "add-generic-password -U -a {} -s {} -X {}\n",
        quote(account),
        quote(service),
        hex
    );
    (line.len() <= STDIN_LINE_LIMIT).then_some(line)
}

/// Create the Keychain item for `service` and `account`, or replace its
/// value, by running `security`.
///
/// The value only ever travels on `security`'s stdin, never on its command
/// line, where every user of the machine could read it from the process
/// list. `security -i` reads a command into a fixed buffer, so a value that
/// does not fit is not written at all: the answer is [`Written::TooLarge`]
/// and the caller stores it in an owner-only file instead. Fails on a
/// non-zero exit or after 8 seconds.
pub fn keychain_set(service: &str, account: &str, value: &str) -> Result<Written> {
    let Some(line) = store_line(service, account, value) else {
        return Ok(Written::TooLarge);
    };
    let out = run_bounded(Command::new(SECURITY).arg("-i"), Some(&line), TIMEOUT)?;
    if out.code != 0 {
        bail!(
            "Keychain write of '{service}' failed (exit {}): {}",
            out.code,
            out.stderr.trim()
        );
    }
    Ok(Written::Stored)
}

/// Store a value too long for [`keychain_set`] in the Keychain item for
/// `service` and `account`, by passing it to `security` as an argument in
/// hexadecimal.
///
/// An argument can be seen in the process list by other users of the
/// machine for the moment the command runs, which is why [`keychain_set`]
/// never does this. It is used for one item only: Claude Code's own login,
/// when it holds more than fits on the tool's standard input (it also
/// carries the sign-ins of MCP servers, tens of kilobytes). Claude Code
/// reads that item and nothing else on macOS, and writes it this same way,
/// so the alternative is to leave Claude Code signed out.
pub fn keychain_set_long(service: &str, account: &str, value: &str) -> Result<()> {
    // Well under the limit on a command's arguments.
    const MAX_HEX: usize = 900_000;
    let hex: String = value.bytes().map(|b| format!("{b:02x}")).collect();
    if hex.len() > MAX_HEX {
        bail!(
            "the login for '{service}' is too large to store ({} bytes)",
            value.len()
        );
    }
    let out = run_bounded(
        Command::new(SECURITY).args(["add-generic-password", "-U", "-a", account, "-s", service, "-X", &hex]),
        None,
        TIMEOUT,
    )?;
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

fn read_file(name: &str) -> Result<Option<String>> {
    match std::fs::read_to_string(secret_file(name)) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("read secret file"),
    }
}

fn write_file(name: &str, value: &str) -> Result<()> {
    let path = secret_file(name);
    util::mkdir_private(path.parent().context("secret path has no parent")?)?;
    util::atomic_write(&path, value.as_bytes(), 0o600)
}

fn delete_file(name: &str) -> Result<()> {
    match std::fs::remove_file(secret_file(name)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("delete secret file"),
    }
}

/// Read one of cx's own secrets: the Keychain item under [`SERVICE`], or
/// the owner-only file `secrets/<name>.secret`. With the Keychain backend
/// the file is looked at when there is no item, because a value too long
/// for the Keychain write is kept there (see [`set`]). `Ok(None)` when it
/// was never stored. The returned value is secret and must not be logged.
pub fn get(name: &str) -> Result<Option<String>> {
    match backend() {
        Backend::Keychain => match keychain_get(SERVICE, name)? {
            Some(text) => Ok(Some(text)),
            None => read_file(name),
        },
        Backend::File => read_file(name),
    }
}

/// Store one of cx's own secrets, replacing any earlier value, and leave
/// exactly one copy of it.
///
/// With the Keychain backend a value that fits goes to the Keychain. One
/// that is too long to pass safely (see [`keychain_set`]) goes to the
/// owner-only file instead, and the Keychain item is removed so an older
/// value cannot be read in its place. Files are written atomically with
/// mode 0600 in a private directory.
pub fn set(name: &str, value: &str) -> Result<()> {
    match backend() {
        Backend::Keychain => match keychain_set(SERVICE, name, value)? {
            Written::Stored => delete_file(name),
            Written::TooLarge => {
                write_file(name, value)?;
                keychain_delete(SERVICE, name)
            }
        },
        Backend::File => write_file(name, value),
    }
}

/// Remove one of cx's own secrets from wherever it is kept. Succeeds when
/// it was already gone.
pub fn delete(name: &str) -> Result<()> {
    if backend() == Backend::Keychain {
        keychain_delete(SERVICE, name)?;
    }
    delete_file(name)
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
        for value in [&small, &small] {
            assert_eq!(keychain_set(service, &user, value).unwrap(), Written::Stored);
            assert_eq!(keychain_get(service, &user).unwrap().as_deref(), Some(value.as_str()));
        }
        // Too long for stdin: refused, and the item is left as it was.
        assert_eq!(keychain_set(service, &user, &big).unwrap(), Written::TooLarge);
        assert_eq!(keychain_get(service, &user).unwrap().as_deref(), Some(small.as_str()));
        // The long form stores it, at the size of a real login that also
        // holds MCP sign-ins, and a later short write replaces it.
        let huge = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"at"}},"mcpOAuth":"{}"}}"#,
            "z\"".repeat(15_000)
        );
        for value in [&big, &huge] {
            keychain_set_long(service, &user, value).unwrap();
            assert_eq!(keychain_get(service, &user).unwrap().as_deref(), Some(value.as_str()));
        }
        assert_eq!(keychain_set(service, &user, &small).unwrap(), Written::Stored);
        assert_eq!(keychain_get(service, &user).unwrap().as_deref(), Some(small.as_str()));
        keychain_delete(service, &user).unwrap();
        assert_eq!(keychain_get(service, &user).unwrap(), None);
        keychain_delete(service, &user).unwrap();
    }

    #[test]
    fn cx_secrets_only_ever_go_to_security_on_stdin() {
        let line = store_line("svc", "me", "ab").unwrap();
        assert_eq!(line, "add-generic-password -U -a \"me\" -s \"svc\" -X 6162\n");
        // One byte under the buffer fits; a long value is refused, not
        // passed some other way.
        assert!(store_line("svc", "me", &"x".repeat(1900)).is_some());
        assert!(store_line("svc", "me", &"x".repeat(2100)).is_none());
        // The only place a value goes on a command line is the long form,
        // which nothing in this module calls: cx's own secrets never use it.
        let source = include_str!("secrets.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        assert_eq!(
            code.matches("\"-X\"").count(),
            1,
            "a value is being put on a command line"
        );
        assert_eq!(
            code.matches("keychain_set_long(").count(),
            1,
            "only its definition may appear here"
        );
    }
}
