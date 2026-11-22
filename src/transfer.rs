//! `cx export` and `cx import`: every account, its login, the settings and
//! the directory pins in one file, encrypted with a passphrase.
//!
//! The file is in the age format with ASCII armour, so `age -d` opens it
//! without cx. It holds refresh tokens. A refresh token works once: after a
//! restore on a second machine, whichever machine refreshes an account
//! first retires the other's copy.

use crate::engine::Engine;
use crate::secrets;
use crate::store::{Account, Kind, Settings};
use crate::util::now_ms;
use age::secrecy::SecretString;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};

#[derive(Serialize, Deserialize)]
struct Entry {
    account: Account,
    secret: String,
}

#[derive(Serialize, Deserialize)]
struct Bundle {
    cx_export: u32,
    created_ms: i64,
    accounts: Vec<Entry>,
    settings: Settings,
    #[serde(default)]
    mappings: BTreeMap<String, u32>,
}

fn encrypt(plain: &[u8], passphrase: &str) -> Result<Vec<u8>> {
    let encryptor = age::Encryptor::with_user_passphrase(SecretString::from(passphrase.to_owned()));
    let mut out = Vec::new();
    let armor = age::armor::ArmoredWriter::wrap_output(&mut out, age::armor::Format::AsciiArmor)?;
    let mut writer = encryptor.wrap_output(armor)?;
    writer.write_all(plain)?;
    writer.finish()?.finish()?;
    Ok(out)
}

fn decrypt(sealed: &[u8], passphrase: &str) -> Result<Vec<u8>> {
    let decryptor =
        age::Decryptor::new(age::armor::ArmoredReader::new(sealed)).context("this is not a cx export file")?;
    let identity = age::scrypt::Identity::new(SecretString::from(passphrase.to_owned()));
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|e| match e {
            age::DecryptError::DecryptionFailed
            | age::DecryptError::KeyDecryptionFailed
            | age::DecryptError::NoMatchingKeys => anyhow!("wrong passphrase"),
            other => anyhow!("could not open the file: {other}"),
        })?;
    let mut plain = Vec::new();
    reader.read_to_end(&mut plain)?;
    Ok(plain)
}

/// The encrypted export, and how many accounts it holds.
pub fn export(engine: &Engine, passphrase: &str) -> Result<(Vec<u8>, usize)> {
    if passphrase.chars().count() < 8 {
        bail!("the passphrase needs at least 8 characters: the file holds every account's login");
    }
    // Take the newest copy of each login first; Claude Code may have
    // refreshed one since cx last looked.
    let live = engine.identify_live(false)?;
    let state = engine.store.load()?;
    for account in state.accounts.values().filter(|a| a.kind == Kind::Oauth) {
        let _ = engine.converge(account.id, &live);
    }
    let state = engine.store.load()?;
    let mut accounts = Vec::new();
    for account in state.accounts.values() {
        let secret = secrets::get(&account.secret_name())?
            .with_context(|| format!("{} has no stored login", account.display()))?;
        accounts.push(Entry {
            account: account.clone(),
            secret,
        });
    }
    let bundle = Bundle {
        cx_export: 1,
        created_ms: now_ms(),
        accounts,
        settings: state.settings.clone(),
        mappings: state.mappings.clone(),
    };
    let sealed = encrypt(&serde_json::to_vec(&bundle)?, passphrase)?;
    Ok((sealed, bundle.accounts.len()))
}

/// What [`import`] did with one account from an export.
pub struct Restored {
    /// The account's label as stored in the export.
    pub label: String,
    /// One line saying what happened, for the report.
    pub outcome: String,
}

/// Bring the accounts in an export into this store. An account cx already
/// has keeps its number and is only overwritten by a newer login. Settings
/// and directory pins come over only when `with_settings` is set.
pub fn import(engine: &Engine, sealed: &[u8], passphrase: &str, with_settings: bool) -> Result<Vec<Restored>> {
    let bundle: Bundle =
        serde_json::from_slice(&decrypt(sealed, passphrase)?).context("the file opened but is not a cx export")?;
    if bundle.cx_export != 1 {
        bail!(
            "this export is version {}, which this cx does not read",
            bundle.cx_export
        );
    }
    // The file is someone's input: its settings are checked like typed ones
    // before anything is changed.
    if with_settings {
        bundle
            .settings
            .validate()
            .context("the file's settings are not valid")?;
    }
    let _operation = engine.store.lock_operations()?;
    let mut report = Vec::new();
    // Old number -> new number, for the directory pins.
    let mut renumbered = BTreeMap::new();
    for entry in bundle.accounts {
        let incoming = entry.account;
        let oauth = crate::cred::Oauth::from_credential(&entry.secret);
        let state = engine.store.load()?;
        let existing = state
            .accounts
            .values()
            .find(|a| match incoming.kind {
                Kind::Oauth => {
                    a.kind == Kind::Oauth
                        && !incoming.uuid.is_empty()
                        && a.uuid == incoming.uuid
                        && a.org_uuid == incoming.org_uuid
                }
                kind => a.kind == kind && a.label == incoming.label,
            })
            .cloned();
        let label = incoming.label.clone();
        if let Some(mine) = &existing {
            renumbered.insert(incoming.id, mine.id);
            let newer = match &oauth {
                Some(o) => o.expires_ms() > mine.token_expires_ms,
                None => secrets::get(&mine.secret_name())?.as_deref() != Some(entry.secret.as_str()),
            };
            if !newer {
                report.push(Restored {
                    label,
                    outcome: format!("already here as #{} with the same or a newer login", mine.id),
                });
                continue;
            }
        }
        let id = engine.store.update(|state| {
            let id = match &existing {
                Some(mine) => mine.id,
                None if !state.accounts.contains_key(&incoming.id) => incoming.id,
                None => state.next_id(),
            };
            let mut account = incoming.clone();
            account.id = id;
            if let Some(mine) = &existing {
                // What was set on this machine stays.
                account.alias = mine.alias.clone().or(account.alias);
                account.disabled = mine.disabled;
                account.priority = mine.priority;
                account.added_ms = mine.added_ms;
            }
            // An alias another account here already uses is not taken over.
            if state
                .accounts
                .values()
                .any(|a| a.id != id && a.alias.is_some() && a.alias == account.alias)
            {
                account.alias = None;
            }
            state.accounts.insert(id, account);
            Ok(id)
        })?;
        renumbered.insert(incoming.id, id);
        match &oauth {
            Some(o) => engine.write_secret(id, o)?,
            None => secrets::set(&format!("acct-{id}"), entry.secret.trim())?,
        }
        report.push(Restored {
            label,
            outcome: match existing {
                Some(_) => format!("updated #{id} with the newer login"),
                None => format!("added as #{id}"),
            },
        });
    }
    if with_settings {
        engine.store.update(|state| {
            state.settings = bundle.settings.clone();
            for (dir, old) in &bundle.mappings {
                if let Some(id) = renumbered.get(old) {
                    state.mappings.insert(dir.clone(), *id);
                }
            }
            Ok(())
        })?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_file_opens_only_with_its_passphrase() {
        let sealed = encrypt(b"the logins", "correct horse").unwrap();
        assert!(sealed.starts_with(b"-----BEGIN AGE ENCRYPTED FILE-----"));
        assert!(!String::from_utf8_lossy(&sealed).contains("the logins"));
        assert_eq!(decrypt(&sealed, "correct horse").unwrap(), b"the logins");
        let wrong = decrypt(&sealed, "battery staple").unwrap_err().to_string();
        assert_eq!(wrong, "wrong passphrase");
        assert!(decrypt(b"hello", "x").is_err());
    }
}
