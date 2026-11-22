//! Ways an account gets into cx: a browser sign-in, the login Claude Code
//! already holds, a pasted key, or an import from claude-swap.

use crate::claude::{AUTH_OVERRIDE_ENV, ClaudeHome};
use crate::cred::{self, Oauth, TokenKind};
use crate::engine::Engine;
use crate::run::claude_binary;
use crate::secrets::{self, Backend};
use crate::store::{Account, Kind};
use crate::util::{self, now_ms};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Sign in through the browser.
///
/// Claude Code's own `claude auth login` does the sign-in, pointed at a
/// throwaway config directory so the default login is not disturbed. cx then
/// moves the new login into its store and deletes the throwaway. It never
/// runs `claude auth logout` there: that would revoke the token it just took.
pub fn login(engine: &Engine, email: Option<&str>, sso: bool) -> Result<(u32, bool)> {
    let scratch = ClaudeHome::at(
        util::store_root()
            .join("login")
            .join(format!("{}-{}", now_ms(), fastrand::u32(..))),
    );
    util::mkdir_private(&scratch.dir)?;
    let result = (|| -> Result<(u32, bool)> {
        let mut command = Command::new(claude_binary());
        command.args(["auth", "login", "--claudeai"]);
        if let Some(email) = email {
            command.args(["--email", email]);
        }
        if sso {
            command.arg("--sso");
        }
        for name in AUTH_OVERRIDE_ENV {
            command.env_remove(name);
        }
        command.env("CLAUDE_CONFIG_DIR", scratch.config_dir_value());
        let status = command
            .status()
            .with_context(|| format!("could not start `{}`; is Claude Code installed?", claude_binary()))?;
        let oauth = scratch.read_oauth()?;
        let Some(oauth) = oauth else {
            if status.success() {
                bail!("Claude Code reported success but left no login behind");
            }
            bail!("the sign-in did not finish");
        };
        engine.adopt(&oauth, scratch.oauth_account(), None)
    })();
    let _ = scratch.delete_credentials();
    let _ = std::fs::remove_dir_all(&scratch.dir);
    result
}

/// Take over the login the default Claude Code home holds now.
pub fn add_current(engine: &Engine) -> Result<(u32, bool)> {
    let home = ClaudeHome::default_home();
    let oauth = home
        .read_oauth()?
        .context("Claude Code is not signed in. Run `cx login` to sign in to an account.")?;
    engine.adopt(&oauth, home.oauth_account(), None)
}

/// Store a Console API key or a setup-token.
pub fn add_key(engine: &Engine, key: &str, label: Option<&str>) -> Result<Account> {
    let key = key.trim();
    let kind = match cred::classify_token(key) {
        Some(TokenKind::ApiKey) => Kind::ApiKey,
        Some(TokenKind::SetupToken) => Kind::SetupToken,
        None => bail!("that is not an Anthropic key (it should start with sk-ant-)"),
    };
    let _operation = engine.store.lock_operations()?;
    let account = engine.store.update(|state| {
        let id = state.next_id();
        let fallback = match kind {
            Kind::ApiKey => format!("api-key-{id}"),
            _ => format!("setup-token-{id}"),
        };
        let mut account = Account::new(
            id,
            kind,
            label.map(str::trim).filter(|l| !l.is_empty()).unwrap_or(&fallback),
        );
        account.hint = key
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        state.accounts.insert(id, account.clone());
        Ok(account)
    })?;
    // An account without its key is useless: take it back out if the key
    // could not be stored.
    if let Err(error) = secrets::set(&account.secret_name(), key) {
        let _ = engine.store.update(|state| {
            state.accounts.remove(&account.id);
            Ok(())
        });
        return Err(error);
    }
    Ok(account)
}

// ---- import from claude-swap -------------------------------------------

/// What [`import_cswap`] did, or in a dry run would do, with one claude-swap
/// account.
pub struct Imported {
    /// The account's slot number in claude-swap. 0 when the slot key is not
    /// a number.
    pub slot: u32,
    /// The account's email address as claude-swap recorded it.
    pub label: String,
    /// One line saying what happened, for the report.
    pub outcome: String,
    /// False when the account could not be imported.
    pub ok: bool,
}

fn cswap_secret(root: &Path, slot: &str, email: &str) -> Result<Option<String>> {
    if secrets::backend() == Backend::Keychain
        && let Some(text) = secrets::keychain_get("claude-swap", &format!("account-{slot}-{email}"))?
    {
        return Ok(Some(text));
    }
    // File storage, used off macOS and as claude-swap's fallback: base64.
    let file = root.join("credentials").join(format!(".creds-{slot}-{email}.enc"));
    match std::fs::read_to_string(&file) {
        Ok(text) => Ok(decode_base64(text.trim())),
        Err(_) => Ok(None),
    }
}

fn decode_base64(text: &str) -> Option<String> {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bytes = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0u8);
    for c in text.bytes().filter(|c| *c != b'=' && !c.is_ascii_whitespace()) {
        let value = TABLE.iter().position(|t| *t == c)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    String::from_utf8(bytes).ok()
}

/// Where claude-swap keeps its data: `~/.claude-swap-backup` when that
/// exists, otherwise `claude-swap` under `XDG_DATA_HOME` (by default
/// `~/.local/share`). The directory returned may not exist.
pub fn cswap_root() -> PathBuf {
    let legacy = util::home().join(".claude-swap-backup");
    if legacy.exists() {
        return legacy;
    }
    let xdg = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|p| p.starts_with('/'))
        .map(PathBuf::from)
        .unwrap_or_else(|| util::home().join(".local").join("share"));
    xdg.join("claude-swap")
}

/// Copy every account from a claude-swap store into cx.
///
/// claude-swap's own data is read and never changed. After the import the
/// two tools hold the same refresh tokens, and the first refresh by either
/// one retires the other's copy, so only one of them should stay in use.
pub fn import_cswap(engine: &Engine, root: &Path, dry_run: bool) -> Result<Vec<Imported>> {
    let sequence = util::read_json(&root.join("sequence.json"))?
        .with_context(|| format!("no claude-swap data at {}", root.display()))?;
    let accounts = sequence
        .get("accounts")
        .and_then(Value::as_object)
        .context("sequence.json has no accounts")?;
    let _operation = if dry_run {
        None
    } else {
        Some(engine.store.lock_operations()?)
    };
    let mut report = Vec::new();
    for (slot, entry) in accounts {
        let slot_number: u32 = slot.parse().unwrap_or(0);
        let text = |key: &str| entry.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        let email = text("email");
        let mut row = |outcome: String, ok: bool| {
            report.push(Imported {
                slot: slot_number,
                label: email.clone(),
                outcome,
                ok,
            })
        };
        let secret = match cswap_secret(root, slot, &email) {
            Ok(Some(s)) => s,
            Ok(None) => {
                row("no stored credential found".into(), false);
                continue;
            }
            Err(e) => {
                row(format!("could not read the credential: {e}"), false);
                continue;
            }
        };
        let oauth = Oauth::from_credential(&secret);
        let kind = match (&oauth, cred::classify_token(&secret)) {
            (Some(_), _) => Kind::Oauth,
            (None, Some(TokenKind::ApiKey)) => Kind::ApiKey,
            (None, Some(TokenKind::SetupToken)) => Kind::SetupToken,
            (None, None) => {
                row("the stored credential has an unknown shape".into(), false);
                continue;
            }
        };
        let token_note = match &oauth {
            Some(o) if o.refresh().is_none() => "login without a refresh token".to_string(),
            Some(o) if o.expires_ms() > now_ms() => "login, access token valid".to_string(),
            Some(_) => "login, access token expired (will refresh on first use)".to_string(),
            None if kind == Kind::ApiKey => "API key".to_string(),
            None => "setup token".to_string(),
        };
        if dry_run {
            row(format!("would import: {token_note}"), true);
            continue;
        }
        let config = util::read_json(&root.join("configs").join(format!(".claude-config-{slot}-{email}.json")))
            .ok()
            .flatten();
        let oauth_account = config
            .as_ref()
            .and_then(|c| c.get("oauthAccount"))
            .filter(|v| v.is_object())
            .cloned();
        let (uuid, org_uuid) = (text("uuid"), text("organizationUuid"));
        let existing = engine.store.load()?.accounts.values().find_map(|a| {
            let same = match kind {
                Kind::Oauth => a.kind == Kind::Oauth && !uuid.is_empty() && a.uuid == uuid && a.org_uuid == org_uuid,
                _ => a.kind == kind && a.label == email,
            };
            same.then_some(a.id)
        });
        // An account cx already has is only overwritten by a newer login.
        if let (Some(id), Some(incoming)) = (existing, &oauth) {
            let current = engine
                .store
                .load()?
                .accounts
                .get(&id)
                .map(|a| a.token_expires_ms)
                .unwrap_or(0);
            if current >= incoming.expires_ms() {
                row(format!("already in cx as #{id} with the same or a newer login"), true);
                continue;
            }
        }
        let id = engine.store.update(|state| {
            let id = existing.unwrap_or_else(|| {
                if slot_number > 0 && !state.accounts.contains_key(&slot_number) {
                    slot_number
                } else {
                    state.next_id()
                }
            });
            let account = state
                .accounts
                .entry(id)
                .or_insert_with(|| Account::new(id, kind, &email));
            account.label = email.clone();
            account.uuid = uuid.clone();
            account.org_uuid = org_uuid.clone();
            account.org_name = text("organizationName");
            account.disabled = entry.get("disabled").and_then(Value::as_bool).unwrap_or(false);
            account.alias = entry
                .get("alias")
                .and_then(Value::as_str)
                .map(String::from)
                .or(account.alias.take());
            if oauth_account.is_some() {
                account.oauth_account = oauth_account.clone();
            }
            Ok(id)
        })?;
        match &oauth {
            Some(o) => engine.write_secret(id, o)?,
            None => secrets::set(&format!("acct-{id}"), secret.trim())?,
        }
        row(format!("imported as #{id}: {token_note}"), true);
    }
    report.sort_by_key(|r| r.slot);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decodes_with_and_without_padding() {
        assert_eq!(decode_base64("aGVsbG8=").as_deref(), Some("hello"));
        assert_eq!(decode_base64("aGVsbG8h").as_deref(), Some("hello!"));
        assert_eq!(decode_base64("eyJhIjoxfQ==").as_deref(), Some("{\"a\":1}"));
        assert_eq!(decode_base64("***"), None);
    }
}
