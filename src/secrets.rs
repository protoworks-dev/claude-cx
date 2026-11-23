//! Secret storage. Where cx keeps each account's login or key:
//!
//! - the macOS Keychain, through `/usr/bin/security`;
//! - on a system without a Keychain, [age](https://age-encryption.org)
//!   files, one per secret, encrypted to a key file (see [`age_key_file`]);
//! - `CX_SECRETS=fnox`: [fnox](https://fnox.jdx.dev), with any provider fnox
//!   has;
//! - `CX_SECRETS=file`: owner-only files that are not encrypted.
//!
//! A value never goes on a command line. One too long for `security`'s
//! stdin is kept in an owner-only file instead, and fnox is handed a value
//! on its stdin.
//!
//! The Keychain is driven through Apple's `security` tool and not through
//! Security.framework. An item's access list names the program that created
//! it, and `security` is the same binary on every run, so reads stay silent
//! after cx is rebuilt. Claude Code reads and writes its own items the same
//! way, which is why cx can read them without a prompt.
//!
//! This covers cx's own copies only. Claude Code keeps the login it is using
//! in its own place: the Keychain on macOS, and a plain `.credentials.json`
//! in its config home everywhere else. cx cannot change that.

use crate::lock::DirLock;
use crate::util::{self, run_bounded};
use age::secrecy::ExposeSecret;
use anyhow::{Context, Result, anyhow, bail};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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
    /// One age file per secret, `secrets/<name>.age` in the cx state
    /// directory, encrypted to the key file [`age_key_file`] names.
    Age,
    /// The `fnox` command, with the config file [`fnox_config`] names.
    Fnox,
    /// One file per secret under `secrets/` in the cx state directory,
    /// readable by the owner only (mode 0600) and not encrypted.
    File,
}

/// The backend in use. `CX_SECRETS` chooses one: `file`, `age` or `fnox`.
/// Without it, [`Backend::Keychain`] on macOS and [`Backend::Age`] on every
/// other platform. The environment is read on every call.
///
/// Any backend other than the Keychain also keeps cx away from Claude
/// Code's own Keychain items, which is what the tests rely on.
pub fn backend() -> Backend {
    match std::env::var("CX_SECRETS").unwrap_or_default().as_str() {
        "file" => Backend::File,
        "age" => Backend::Age,
        "fnox" => Backend::Fnox,
        _ if cfg!(target_os = "macos") => Backend::Keychain,
        _ => Backend::Age,
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

fn secrets_dir() -> PathBuf {
    util::store_root().join("secrets")
}

fn secret_file(name: &str) -> PathBuf {
    secrets_dir().join(format!("{name}.secret"))
}

fn age_file(name: &str) -> PathBuf {
    secrets_dir().join(format!("{name}.age"))
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

fn remove(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("delete {}", path.display())),
    }
}

fn delete_file(name: &str) -> Result<()> {
    remove(&secret_file(name))
}

/// Serializes writes to the age and fnox stores across cx processes. It is
/// taken last and nothing else is taken under it.
fn write_lock() -> Result<DirLock> {
    DirLock::acquire(
        &util::store_root().join("secrets.lock"),
        Duration::from_secs(30),
        Duration::from_secs(20),
    )
    .context("another cx is writing a secret")
}

// ---- age ---------------------------------------------------------------

/// Where an age key came from, for [`age_key_file`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KeySource {
    /// Named by this environment variable.
    Env(&'static str),
    /// A key file another tool keeps (fnox, mise or sops), found in place.
    Shared,
    /// cx's own key, `age.key` in the cx state directory. cx creates it.
    Own,
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The age key file cx encrypts its secrets to and opens them with, and
/// where that choice came from. In order:
///
/// 1. `CX_AGE_KEY_FILE`;
/// 2. the file fnox, sops or mise is told to use: `FNOX_AGE_KEY_FILE`,
///    `SOPS_AGE_KEY_FILE`, `MISE_SOPS_AGE_KEY_FILE`;
/// 3. a key file one of them keeps, when it exists: `~/.config/fnox/age.txt`,
///    `~/.config/mise/age.txt`, `~/.config/sops/age/keys.txt`;
/// 4. cx's own, `age.key` in the cx state directory.
///
/// Only computes the path. cx creates the file only for cases 1 and 4.
pub fn age_key_file() -> (PathBuf, KeySource) {
    for name in [
        "CX_AGE_KEY_FILE",
        "FNOX_AGE_KEY_FILE",
        "SOPS_AGE_KEY_FILE",
        "MISE_SOPS_AGE_KEY_FILE",
    ] {
        if let Some(path) = env_path(name) {
            return (path, KeySource::Env(name));
        }
    }
    let config = util::home().join(".config");
    for path in [
        config.join("fnox").join("age.txt"),
        config.join("mise").join("age.txt"),
        config.join("sops").join("age").join("keys.txt"),
    ] {
        if path.is_file() {
            return (path, KeySource::Shared);
        }
    }
    (util::store_root().join("age.key"), KeySource::Own)
}

/// The X25519 identities in an age key file: every line that starts with
/// `AGE-SECRET-KEY-`. `Ok(None)` when there is no such file. Fails when
/// the file holds no usable key.
fn read_identities(path: &Path) -> Result<Option<Vec<age::x25519::Identity>>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read the age key file {}", path.display())),
    };
    let identities: Vec<age::x25519::Identity> = text
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("AGE-SECRET-KEY-"))
        .filter_map(|line| line.parse().ok())
        .collect();
    if identities.is_empty() {
        bail!(
            "the age key file {} holds no X25519 key (a line starting AGE-SECRET-KEY-)",
            path.display()
        );
    }
    Ok(Some(identities))
}

/// Create a new key file at `path` unless one is there, and return the keys
/// in it. Two processes may race: the file is linked into place, which
/// fails when it exists, and the loser reads the winner's key.
fn create_key(path: &Path) -> Result<Vec<age::x25519::Identity>> {
    let dir = path.parent().context("the age key path has no parent")?;
    if dir == util::store_root() {
        util::mkdir_private(dir)?;
    } else {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let identity = age::x25519::Identity::generate();
    let text = format!(
        "# Created by cx (claude-cx). It opens the logins under {}.\n# public key: {}\n{}\n",
        secrets_dir().display(),
        identity.to_public(),
        identity.to_string().expose_secret()
    );
    let tmp = dir.join(format!(".age-key.cx-{}-{}", std::process::id(), fastrand::u32(..)));
    util::atomic_write(&tmp, text.as_bytes(), 0o600)?;
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("create the age key file {}", path.display())),
    }
    read_identities(path)?.with_context(|| format!("the age key file {} vanished", path.display()))
}

/// The keys to use, for reading (`create` false) or writing. A missing key
/// file is created only when it is cx's to create.
fn identities(create: bool) -> Result<Vec<age::x25519::Identity>> {
    let (path, source) = age_key_file();
    if let Some(found) = read_identities(&path)? {
        return Ok(found);
    }
    let creatable = matches!(source, KeySource::Own | KeySource::Env("CX_AGE_KEY_FILE"));
    if create && creatable {
        return create_key(&path);
    }
    match source {
        KeySource::Env(name) if !creatable => bail!("{name} names {}, which does not exist", path.display()),
        _ => bail!(
            "the age key file {} is missing, so the stored logins cannot be opened. Restore it, or sign in again with `cx login`.",
            path.display()
        ),
    }
}

/// Encrypt `plain` to the public halves of `identities`, with ASCII armour,
/// so that `age -d -i <key file>` opens the result without cx.
fn seal(identities: &[age::x25519::Identity], plain: &str) -> Result<Vec<u8>> {
    let recipients: Vec<age::x25519::Recipient> = identities.iter().map(|i| i.to_public()).collect();
    let encryptor = age::Encryptor::with_recipients(recipients.iter().map(|r| r as &dyn age::Recipient))
        .map_err(|e| anyhow!("age could not encrypt: {e}"))?;
    let mut out = Vec::new();
    let armor = age::armor::ArmoredWriter::wrap_output(&mut out, age::armor::Format::AsciiArmor)?;
    let mut writer = encryptor.wrap_output(armor)?;
    writer.write_all(plain.as_bytes())?;
    writer.finish()?.finish()?;
    Ok(out)
}

/// Decrypt an age file, armoured or not, with any of `identities`.
fn open(identities: &[age::x25519::Identity], sealed: &[u8]) -> Result<String> {
    let decryptor = age::Decryptor::new(age::armor::ArmoredReader::new(sealed)).context("not an age file")?;
    let mut reader = decryptor
        .decrypt(identities.iter().map(|i| i as &dyn age::Identity))
        .map_err(|e| match e {
            age::DecryptError::NoMatchingKeys => anyhow!("it was encrypted to another key"),
            other => anyhow!("{other}"),
        })?;
    let mut plain = String::new();
    reader.read_to_string(&mut plain)?;
    Ok(plain)
}

fn read_age(name: &str) -> Result<Option<String>> {
    let path = age_file(name);
    let sealed = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let plain = open(&identities(false)?, &sealed).with_context(|| {
        format!(
            "{} could not be opened with the key in {}",
            path.display(),
            age_key_file().0.display()
        )
    })?;
    Ok(Some(plain))
}

fn write_age(name: &str, value: &str) -> Result<()> {
    let sealed = seal(&identities(true)?, value)?;
    let path = age_file(name);
    util::mkdir_private(path.parent().context("secret path has no parent")?)?;
    util::atomic_write(&path, &sealed, 0o600)
}

// ---- fnox --------------------------------------------------------------

const FNOX_TIMEOUT: Duration = Duration::from_secs(20);
/// What fnox prints, with exit 1, for a secret that is not defined.
const FNOX_NOT_FOUND: &str = "secret::not_found";

/// The fnox config file cx's secrets are kept in: `CX_FNOX_CONFIG`, else
/// `fnox.toml` in the cx state directory. cx writes that file the first
/// time it stores a secret, with one age provider on [`age_key_file`]. A
/// file that is already there is used as it is, so it may name any provider
/// fnox has; `CX_FNOX_PROVIDER` picks one that is not its default.
pub fn fnox_config() -> PathBuf {
    env_path("CX_FNOX_CONFIG").unwrap_or_else(|| util::store_root().join("fnox.toml"))
}

/// The name a cx secret has in fnox: `acct-3.prev` is `CX_ACCT_3_PREV`.
/// fnox names secrets after environment variables.
fn fnox_key(name: &str) -> String {
    let body: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("CX_{body}")
}

fn fnox(config: &Path) -> Command {
    let bin = std::env::var("CX_FNOX_BIN")
        .ok()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "fnox".into());
    let mut command = Command::new(bin);
    // cx cannot answer a prompt, so fnox must not wait for one.
    command.arg("-c").arg(config).args(["--non-interactive", "--no-color"]);
    command
}

fn fnox_failure(what: &str, out: &util::Output) -> anyhow::Error {
    let reason = out
        .stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no message");
    anyhow!("fnox could not {what} (exit {}): {reason}", out.code)
}

fn fnox_run(command: &mut Command, stdin: Option<&str>) -> Result<util::Output> {
    run_bounded(command, stdin, FNOX_TIMEOUT)
        .context("`fnox` could not be run. Install fnox, set CX_FNOX_BIN, or choose another store with CX_SECRETS")
}

fn toml_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Write cx's own fnox config when there is none: one age provider on the
/// key [`age_key_file`] names.
fn fnox_ensure_config(config: &Path) -> Result<()> {
    if config.exists() {
        return Ok(());
    }
    let keys = identities(true)?;
    let recipients: Vec<String> = keys.iter().map(|k| toml_string(&k.to_public().to_string())).collect();
    let text = format!(
        "# Written by cx (claude-cx). Each login is a secret named CX_ACCT_<number>.\n\
         # Any fnox provider can take the place of this one: https://fnox.jdx.dev\n\
         default_provider = \"cx\"\n\n\
         [providers.cx]\n\
         type = \"age\"\n\
         recipients = [{}]\n\
         key_file = {}\n",
        recipients.join(", "),
        toml_string(&age_key_file().0.to_string_lossy())
    );
    util::atomic_write(config, text.as_bytes(), 0o600)
}

fn fnox_get(name: &str) -> Result<Option<String>> {
    let config = fnox_config();
    if !config.exists() {
        return Ok(None);
    }
    let out = fnox_run(fnox(&config).args(["get", fnox_key(name).as_str()]), None)?;
    if out.code == 0 {
        return Ok(Some(out.stdout.strip_suffix('\n').unwrap_or(&out.stdout).to_string()));
    }
    if out.stderr.contains(FNOX_NOT_FOUND) {
        return Ok(None);
    }
    Err(fnox_failure("read a secret", &out))
}

fn fnox_set(name: &str, value: &str) -> Result<()> {
    let config = fnox_config();
    fnox_ensure_config(&config)?;
    let mut command = fnox(&config);
    command.args(["set", fnox_key(name).as_str()]);
    if let Some(provider) = std::env::var("CX_FNOX_PROVIDER").ok().filter(|p| !p.is_empty()) {
        command.args(["-p", provider.as_str()]);
    }
    // The value goes on stdin. fnox drops one final newline, so none is sent.
    let out = fnox_run(&mut command, Some(value))?;
    if out.code != 0 {
        return Err(fnox_failure("store a secret", &out));
    }
    Ok(())
}

fn fnox_delete(name: &str) -> Result<()> {
    let config = fnox_config();
    if !config.exists() {
        return Ok(());
    }
    let out = fnox_run(fnox(&config).args(["remove", fnox_key(name).as_str()]), None)?;
    if out.code == 0 || out.stderr.contains(FNOX_NOT_FOUND) {
        return Ok(());
    }
    Err(fnox_failure("remove a secret", &out))
}

// ---- cx's own secrets --------------------------------------------------

/// The copy in the backend's own place, without looking anywhere older.
fn get_primary(backend: Backend, name: &str) -> Result<Option<String>> {
    match backend {
        Backend::Keychain => keychain_get(SERVICE, name),
        Backend::Age => read_age(name),
        Backend::Fnox => fnox_get(name),
        Backend::File => read_file(name),
    }
}

/// A copy left by an earlier backend: the age file when fnox is in use,
/// then the plain file. Before 0.6 every system without a Keychain used
/// plain files.
fn get_older(backend: Backend, name: &str) -> Result<Option<String>> {
    if backend == Backend::Fnox
        && let Some(text) = read_age(name)?
    {
        return Ok(Some(text));
    }
    match backend {
        Backend::File => Ok(None),
        _ => read_file(name),
    }
}

/// Read one of cx's own secrets from the backend in use. `Ok(None)` when it
/// was never stored. The returned value is secret and must not be logged.
///
/// When the backend has no copy, older places are looked at: the plain file
/// `secrets/<name>.secret` (with the Keychain, a value too long for the
/// Keychain write is kept there, see [`set`]; elsewhere it is what cx wrote
/// before it encrypted), and with fnox also the age file.
pub fn get(name: &str) -> Result<Option<String>> {
    let backend = backend();
    if let Some(text) = get_primary(backend, name)? {
        return Ok(Some(text));
    }
    if let Some(text) = get_older(backend, name)? {
        return Ok(Some(text));
    }
    // A writer stores the new copy and then removes the older one. A read
    // that fell between the two saw neither, and finds the new copy now.
    match backend {
        Backend::Age | Backend::Fnox => get_primary(backend, name),
        _ => Ok(None),
    }
}

/// Store one of cx's own secrets, replacing any earlier value, and leave
/// exactly one copy of it.
///
/// With the Keychain backend a value that fits goes to the Keychain. One
/// that is too long to pass safely (see [`keychain_set`]) goes to the
/// owner-only file instead, and the Keychain item is removed so an older
/// value cannot be read in its place. With age or fnox the value is
/// encrypted first and any copy an earlier backend left is then removed.
/// Files are written atomically with mode 0600 in a private directory.
pub fn set(name: &str, value: &str) -> Result<()> {
    match backend() {
        Backend::Keychain => match keychain_set(SERVICE, name, value)? {
            Written::Stored => delete_file(name),
            Written::TooLarge => {
                write_file(name, value)?;
                keychain_delete(SERVICE, name)
            }
        },
        Backend::Age => {
            let _lock = write_lock()?;
            write_age(name, value)?;
            delete_file(name)
        }
        Backend::Fnox => {
            let _lock = write_lock()?;
            fnox_set(name, value)?;
            remove(&age_file(name))?;
            delete_file(name)
        }
        Backend::File => write_file(name, value),
    }
}

/// Remove one of cx's own secrets from wherever it is kept, older places
/// included. Succeeds when it was already gone.
pub fn delete(name: &str) -> Result<()> {
    match backend() {
        Backend::Keychain => keychain_delete(SERVICE, name)?,
        Backend::Age => {
            let _lock = write_lock()?;
            remove(&age_file(name))?;
        }
        Backend::Fnox => {
            let _lock = write_lock()?;
            fnox_delete(name)?;
            remove(&age_file(name))?;
        }
        Backend::File => {}
    }
    delete_file(name)
}

/// Move what an earlier backend left into the one in use: with age, the
/// plain files cx wrote before it encrypted; with fnox, those and the age
/// files. Returns how many secrets were moved. Does nothing with the
/// Keychain or plain files.
///
/// A secret the backend already holds is newer than the leftover, so the
/// leftover is only removed. Each secret is moved under the lock every
/// write takes, so a login being renewed at the same moment is not undone.
pub fn encrypt_leftovers() -> Result<usize> {
    let backend = backend();
    let suffixes: &[&str] = match backend {
        Backend::Age => &[".secret"],
        Backend::Fnox => &[".age", ".secret"],
        Backend::Keychain | Backend::File => return Ok(0),
    };
    let entries = match std::fs::read_dir(secrets_dir()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).context("list the secrets directory"),
    };
    let mut names = std::collections::BTreeSet::new();
    for entry in entries.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if file.starts_with('.') {
            continue;
        }
        if let Some(name) = suffixes.iter().find_map(|s| file.strip_suffix(*s)) {
            names.insert(name.to_string());
        }
    }
    let mut moved = 0;
    for name in names {
        let _lock = write_lock()?;
        if get_primary(backend, &name)?.is_none() {
            let Some(value) = get_older(backend, &name)? else {
                continue;
            };
            match backend {
                Backend::Age => write_age(&name, &value)?,
                _ => fnox_set(&name, &value)?,
            }
            moved += 1;
        }
        if backend == Backend::Fnox {
            remove(&age_file(&name))?;
        }
        delete_file(&name)?;
    }
    Ok(moved)
}

/// Where cx's secrets are on this machine, in words for a message: "the
/// macOS Keychain (service 'cx-accounts')", or the directory or file.
pub fn place() -> String {
    match backend() {
        Backend::Keychain => format!("the macOS Keychain (service '{SERVICE}')"),
        Backend::Age => format!("age-encrypted files under {}", secrets_dir().display()),
        Backend::Fnox => format!("fnox ({})", fnox_config().display()),
        Backend::File => format!("unencrypted files under {}", secrets_dir().display()),
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cx-secrets-{name}-{}-{}",
            std::process::id(),
            fastrand::u32(..)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_age_file_opens_with_its_key_and_no_other() {
        let (mine, other) = (age::x25519::Identity::generate(), age::x25519::Identity::generate());
        let login = r#"{"claudeAiOauth":{"accessToken":"at-secret","refreshToken":"rt-secret"}}"#;
        let sealed = seal(std::slice::from_ref(&mine), login).unwrap();
        assert!(sealed.starts_with(b"-----BEGIN AGE ENCRYPTED FILE-----"));
        assert!(!String::from_utf8_lossy(&sealed).contains("secret"));
        assert_eq!(open(std::slice::from_ref(&mine), &sealed).unwrap(), login);
        // Any key in the file opens it, as with a key file that was added to.
        assert_eq!(open(&[other, mine], &sealed).unwrap(), login);
        let stranger = age::x25519::Identity::generate();
        let refused = open(&[stranger], &sealed).unwrap_err().to_string();
        assert_eq!(refused, "it was encrypted to another key");
        assert!(open(&[age::x25519::Identity::generate()], b"plain text").is_err());
    }

    #[test]
    fn a_key_file_is_created_once_and_read_the_way_age_keygen_writes_it() {
        let dir = scratch("key");
        let path = dir.join("age.key");
        assert!(read_identities(&path).unwrap().is_none());
        let first = create_key(&path).unwrap();
        // A second creation, as from a racing process, keeps the first key.
        let second = create_key(&path).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].to_public().to_string(), second[0].to_public().to_string());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(
            std::fs::read_dir(&dir).unwrap().count() == 1,
            "no temporary file is left"
        );

        // age-keygen's layout: comments, then the key; a second key below.
        let extra = age::x25519::Identity::generate();
        let text = format!(
            "# created: 2026-01-01T00:00:00Z\n# public key: {}\n{}\n\n  {}  \n",
            first[0].to_public(),
            first[0].to_string().expose_secret(),
            extra.to_string().expose_secret()
        );
        std::fs::write(&path, text).unwrap();
        assert_eq!(read_identities(&path).unwrap().unwrap().len(), 2);
        std::fs::write(&path, "# no key here\n").unwrap();
        assert!(read_identities(&path).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fnox_names_are_environment_variable_names() {
        assert_eq!(fnox_key("acct-3"), "CX_ACCT_3");
        assert_eq!(fnox_key("acct-3.prev"), "CX_ACCT_3_PREV");
        assert_eq!(
            fnox_key("acct-12.reply-1700000000000"),
            "CX_ACCT_12_REPLY_1700000000000"
        );
        assert_eq!(toml_string(r#"/a "b"\c"#), r#""/a \"b\"\\c""#);
    }
}
