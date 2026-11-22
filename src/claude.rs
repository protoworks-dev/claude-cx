//! A Claude Code config home: where one Claude Code profile keeps its login.
//!
//! There are two kinds. The default home is `~/.claude` with its global
//! config at `~/.claude.json`; every plain `claude` uses it. A profile home
//! is a directory cx passes as `CLAUDE_CONFIG_DIR`, with its config inside
//! it; `cx run` uses one per account.
//!
//! On macOS Claude Code keeps the login in the Keychain. The default home's
//! item is `Claude Code-credentials`. A profile's item adds the first eight
//! hex digits of the SHA-256 of the `CLAUDE_CONFIG_DIR` string. A plaintext
//! `.credentials.json` in the home is the fallback, and the only store on
//! other platforms.

use crate::cred::{self, Oauth};
use crate::lock::{CONFIG_STALE, CREDENTIAL_STALE, DirLock};
use crate::secrets::{self, Backend};
use crate::util;
use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::Duration;
use unicode_normalization::UnicodeNormalization;

/// The Keychain service name of the default home's login item, and the
/// prefix of every profile home's.
pub const DEFAULT_SERVICE: &str = "Claude Code-credentials";

/// Environment variables that make Claude Code ignore its stored login.
pub const AUTH_OVERRIDE_ENV: [&str; 5] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
];

/// One Claude Code config home, default or profile. Building one touches
/// nothing on disk.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeHome {
    /// The home directory: `~/.claude`, or the directory passed as
    /// `CLAUDE_CONFIG_DIR`.
    pub dir: PathBuf,
    /// Whether this is the default home. It decides the Keychain item name
    /// and where the config file is.
    pub default: bool,
}

/// Both of a home's credential locks, from
/// [`ClaudeHome::lock_credentials`]. They are released when this is dropped.
pub struct CredentialLocks {
    _legacy: DirLock,
    _primary: DirLock,
}

impl ClaudeHome {
    /// The home every plain `claude` uses: `~/.claude`.
    pub fn default_home() -> ClaudeHome {
        ClaudeHome {
            dir: util::home().join(".claude"),
            default: true,
        }
    }

    /// A profile home at `dir`. The path is used as given, and its exact
    /// string decides the Keychain item name, so pass the same spelling
    /// every time.
    pub fn at(dir: PathBuf) -> ClaudeHome {
        ClaudeHome { dir, default: false }
    }

    /// The profile home cx keeps for an account: `profiles/<account>` in
    /// the cx state directory, where `account` is cx's own number for it.
    pub fn profile(account: u32) -> ClaudeHome {
        ClaudeHome::at(util::store_root().join("profiles").join(account.to_string()))
    }

    /// Whether the home directory is there. A profile home is absent until
    /// something first writes to it.
    pub fn exists(&self) -> bool {
        self.dir.is_dir()
    }

    /// The string exported as `CLAUDE_CONFIG_DIR`. Claude Code hashes this
    /// exact string, so every caller must export exactly this.
    pub fn config_dir_value(&self) -> String {
        self.dir.to_string_lossy().into_owned()
    }

    /// The Keychain service name of this home's login item:
    /// [`DEFAULT_SERVICE`] for the default home, and for a profile that
    /// name plus the first eight hex digits of the SHA-256 of the
    /// NFC-normalized [`config_dir_value`](ClaudeHome::config_dir_value).
    /// Only computes the name; the Keychain is not touched.
    pub fn keychain_service(&self) -> String {
        if self.default {
            return DEFAULT_SERVICE.into();
        }
        let normalized: String = self.config_dir_value().nfc().collect();
        let digest = Sha256::digest(normalized.as_bytes());
        let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
        format!("{DEFAULT_SERVICE}-{hex}")
    }

    /// The path of the plaintext credential file, `.credentials.json` in
    /// the home. The file may not exist. Its contents are secret.
    pub fn credential_file(&self) -> PathBuf {
        self.dir.join(".credentials.json")
    }

    /// The path of the config file Claude Code reads for this home. A
    /// profile keeps `.claude.json` inside the home. The default home uses
    /// `~/.claude.json`, unless a non-empty legacy `.config.json` is in the
    /// home. The file may not exist.
    pub fn config_path(&self) -> PathBuf {
        if !self.default {
            return self.dir.join(".claude.json");
        }
        // Claude Code prefers a non-empty legacy `.config.json` in the home.
        let legacy = self.dir.join(".config.json");
        if std::fs::metadata(&legacy).is_ok_and(|m| m.is_file() && m.len() > 0) {
            return legacy;
        }
        util::home().join(".claude.json")
    }

    /// Take the two locks Claude Code holds around a token refresh, in its
    /// order: `<home>/.oauth_refresh.lock`, then `<home>.lock`.
    pub fn lock_credentials(&self, timeout: Duration) -> Result<CredentialLocks> {
        let primary = DirLock::acquire(&self.dir.join(".oauth_refresh.lock"), CREDENTIAL_STALE, timeout)
            .context("Claude Code is refreshing this login; try again in a few seconds")?;
        let mut name = self.dir.file_name().unwrap_or_default().to_os_string();
        name.push(".lock");
        let legacy = DirLock::acquire(&self.dir.with_file_name(name), CREDENTIAL_STALE, timeout)
            .context("Claude Code is refreshing this login; try again in a few seconds")?;
        Ok(CredentialLocks {
            _legacy: legacy,
            _primary: primary,
        })
    }

    /// Take the lock Claude Code holds while it rewrites the config file: a
    /// directory named after [`config_path`](ClaudeHome::config_path) with
    /// `.lock` appended. Waits up to `timeout` and fails if the lock is
    /// still held then. The lock is released when the result is dropped.
    pub fn lock_config(&self, timeout: Duration) -> Result<DirLock> {
        let path = self.config_path();
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".lock");
        DirLock::acquire(&path.with_file_name(name), CONFIG_STALE, timeout)
    }

    fn keychain_text(&self) -> Result<Option<String>> {
        if secrets::backend() != Backend::Keychain {
            return Ok(None);
        }
        secrets::keychain_get(&self.keychain_service(), &secrets::keychain_user())
    }

    fn file_text(&self) -> Option<String> {
        std::fs::read_to_string(self.credential_file()).ok()
    }

    /// The login this home holds. When the Keychain and the file disagree,
    /// the one that expires later is the one refreshed last.
    pub fn read_oauth(&self) -> Result<Option<Oauth>> {
        let from_keychain = self.keychain_text()?.and_then(|t| Oauth::from_credential(&t));
        let from_file = self.file_text().and_then(|t| Oauth::from_credential(&t));
        Ok(match (from_keychain, from_file) {
            (Some(k), Some(f)) => Some(if f.expires_ms() > k.expires_ms() { f } else { k }),
            (k, f) => k.or(f),
        })
    }

    /// Replace this home's login, keeping the machine-wide members of the
    /// stored object. Call with this home's credential locks held.
    pub fn write_oauth(&self, oauth: &Oauth) -> Result<()> {
        let file = self.credential_file();
        let file_text = self.file_text();
        if secrets::backend() == Backend::Keychain {
            let existing = self.keychain_text()?;
            // A profile that has never been opened has no item yet. Claude
            // Code creates it from the plaintext file on its first write.
            if self.default || existing.is_some() {
                let merged = cred::merge_login(existing.as_deref(), oauth);
                let (service, user) = (self.keychain_service(), secrets::keychain_user());
                match secrets::keychain_set(&service, &user, &merged)? {
                    secrets::Written::Stored => {
                        if file_text.is_none() {
                            return Ok(());
                        }
                    }
                    // Too long to hand to `security` without showing it in
                    // the process list. The whole object goes to the
                    // owner-only file, which Claude Code also reads, and
                    // the item is removed so its older login cannot win.
                    secrets::Written::TooLarge => {
                        util::mkdir_private(&self.dir)?;
                        util::atomic_write(&file, merged.as_bytes(), 0o600)?;
                        return secrets::keychain_delete(&service, &user);
                    }
                }
            }
        }
        util::mkdir_private(&self.dir)?;
        util::atomic_write(&file, cred::merge_login(file_text.as_deref(), oauth).as_bytes(), 0o600)
    }

    /// Remove this home's login from the Keychain and the file.
    pub fn delete_credentials(&self) -> Result<()> {
        if secrets::backend() == Backend::Keychain {
            secrets::keychain_delete(&self.keychain_service(), &secrets::keychain_user())?;
        }
        match std::fs::remove_file(self.credential_file()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// The `oauthAccount` section of this home's config: who Claude Code
    /// believes is logged in.
    pub fn oauth_account(&self) -> Option<Value> {
        util::read_json(&self.config_path())
            .ok()
            .flatten()
            .and_then(|cfg| cfg.get("oauthAccount").cloned())
            .filter(Value::is_object)
    }

    /// Change the config under Claude Code's config lock, leaving every key
    /// the closure does not touch exactly as it was.
    pub fn update_config(&self, change: impl FnOnce(&mut serde_json::Map<String, Value>)) -> Result<()> {
        let _lock = self.lock_config(Duration::from_secs(9))?;
        let path = self.config_path();
        let mut object = match util::read_json(&path)? {
            Some(Value::Object(o)) => o,
            Some(_) => anyhow::bail!("{} is not a JSON object", path.display()),
            None => serde_json::Map::new(),
        };
        change(&mut object);
        util::write_json(&path, &Value::Object(object), 0o600)
    }
}

/// `(account uuid, organization uuid)` from an `oauthAccount` section.
pub fn identity_of(oauth_account: &Value) -> Option<(String, String)> {
    let uuid = oauth_account.get("accountUuid")?.as_str()?.to_string();
    let org = oauth_account
        .get("organizationUuid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    (!uuid.is_empty()).then_some((uuid, org))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_service_name_matches_claude_codes_rule() {
        // The first eight hex digits of the SHA-256 of the exported string,
        // appended to the default name.
        let home = ClaudeHome::at(PathBuf::from("/tmp/profile"));
        let digest = Sha256::digest(b"/tmp/profile");
        let expected: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
        assert_eq!(home.keychain_service(), format!("Claude Code-credentials-{expected}"));
        assert_eq!(home.keychain_service().len(), "Claude Code-credentials-".len() + 8);
    }

    #[test]
    fn identity_needs_an_account_uuid() {
        let v: Value = serde_json::json!({"accountUuid": "u", "organizationUuid": "o"});
        assert_eq!(identity_of(&v), Some(("u".into(), "o".into())));
        assert_eq!(identity_of(&serde_json::json!({"organizationUuid": "o"})), None);
    }
}
