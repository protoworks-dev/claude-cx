//! `cx run`: start Claude Code as one account in this terminal only.
//!
//! Each account gets a profile directory that cx passes as
//! `CLAUDE_CONFIG_DIR`. Claude Code keeps a separate login per config
//! directory, so sessions on different accounts run side by side and the
//! default login is not touched.
//!
//! A profile shares the user's setup by symlink: settings, skills, plugins,
//! hooks and conversation history all point back into `~/.claude`, so a
//! conversation started under one account can be resumed under another.

use crate::claude::{AUTH_OVERRIDE_ENV, ClaudeHome};
use crate::engine::{Engine, Live};
use crate::secrets;
use crate::store::{Account, Kind, State, Strategy};
use crate::ui;
use crate::util;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

/// Keys of Claude Code's config that say who is logged in. They are never
/// copied from the default home into a profile.
const LOGIN_KEYS: [&str; 3] = ["oauthAccount", "primaryApiKey", "customApiKeyResponses"];

/// The Claude Code executable cx starts: the `CX_CLAUDE_BIN` environment
/// variable when set, otherwise `claude` looked up on `PATH`.
pub fn claude_binary() -> String {
    std::env::var("CX_CLAUDE_BIN").unwrap_or_else(|_| "claude".into())
}

fn link_shared(profile: &Path, share: &[String]) -> Result<()> {
    let source_root = ClaudeHome::default_home().dir;
    // Drop links cx made earlier for entries no longer shared.
    if let Ok(entries) = std::fs::read_dir(profile) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let points_home = std::fs::read_link(&path).is_ok_and(|target| target.starts_with(&source_root));
            if points_home && !share.contains(&name) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    for name in share {
        let source = source_root.join(name);
        let dest = profile.join(name);
        if !source.exists() {
            continue;
        }
        match std::fs::symlink_metadata(&dest) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if std::fs::read_link(&dest).is_ok_and(|t| t == source) {
                    continue;
                }
                std::fs::remove_file(&dest)?;
            }
            // Something real is already there. It is the user's, so it stays.
            Ok(_) => {
                ui::note(&format!(
                    "cx: {} in the profile is not shared; it holds its own data",
                    name
                ));
                continue;
            }
            Err(_) => {}
        }
        std::os::unix::fs::symlink(&source, &dest).with_context(|| format!("link {} into the profile", name))?;
    }
    Ok(())
}

fn oauth_account_for(account: &Account) -> Value {
    account.oauth_account.clone().unwrap_or_else(|| {
        json!({
            "accountUuid": account.uuid,
            "emailAddress": account.label,
            "organizationUuid": account.org_uuid,
            "organizationName": account.org_name,
        })
    })
}

/// Create or update the account's profile: shared links and a config that
/// names the account. The login itself is written by `Engine::ensure_fresh`.
pub fn prepare_profile(account: &Account, state: &State) -> Result<ClaudeHome> {
    let profile = ClaudeHome::profile(account.id);
    util::mkdir_private(&profile.dir)?;
    link_shared(&profile.dir, &state.settings.share)?;

    let default_config = match util::read_json(&ClaudeHome::default_home().config_path())? {
        Some(Value::Object(o)) => o,
        _ => serde_json::Map::new(),
    };
    let wanted = oauth_account_for(account);
    profile.update_config(|config| {
        if config.is_empty() {
            // First launch: start from the user's own config so onboarding,
            // theme and per-project trust carry over.
            for (key, value) in &default_config {
                if !LOGIN_KEYS.contains(&key.as_str()) {
                    config.insert(key.clone(), value.clone());
                }
            }
        }
        // MCP servers are managed in the default home and mirrored each launch.
        match default_config.get("mcpServers") {
            Some(servers) => config.insert("mcpServers".into(), servers.clone()),
            None => config.remove("mcpServers"),
        };
        // A project trusted in the default home is trusted here too.
        if let Some(Value::Object(projects)) = default_config.get("projects") {
            let mine = config.entry("projects").or_insert_with(|| json!({}));
            if let Some(mine) = mine.as_object_mut() {
                for (path, settings) in projects {
                    mine.entry(path.clone()).or_insert_with(|| settings.clone());
                }
            }
        }
        config.insert("oauthAccount".into(), wanted);
    })?;
    Ok(profile)
}

/// The account [`choose`] settled on for a `cx run`.
pub struct Launch {
    /// The account to start Claude Code as.
    pub account: Account,
    /// Why this account, in a few words, for the notice on stderr.
    pub why: String,
}

/// Choose the account for a bare `cx run`.
pub fn choose(engine: &Engine, strategy: Option<Strategy>) -> Result<Launch> {
    let snapshot = engine.snapshot(false, true)?;
    if let Some(verdict) = engine.pick(strategy, &snapshot) {
        let account = snapshot.state.accounts[&verdict.id].clone();
        return Ok(Launch {
            account,
            why: verdict.note,
        });
    }
    let state = &snapshot.state;
    if state.settings.api_fallback
        && let Some(key) = state.accounts.values().find(|a| a.kind == Kind::ApiKey && !a.disabled)
    {
        return Ok(Launch {
            account: key.clone(),
            why: "every subscription is at its limit; falling back to an API key, billed per token".into(),
        });
    }
    let soonest = snapshot
        .verdicts
        .iter()
        .filter_map(|v| v.room.as_ref().and_then(|r| r.resets_in_s).filter(|s| *s > 0))
        .min();
    match soonest {
        Some(s) => bail!(
            "every account is at its limit. The first one frees up in {}. (`cx ls` shows them all; `cx config api_fallback true` allows API keys.)",
            util::human_duration(s)
        ),
        None => bail!("no account can be used. Run `cx ls` to see why."),
    }
}

/// Replace this process with Claude Code running as the account.
pub fn exec(engine: &Engine, launch: &Launch, claude_args: &[String]) -> Result<()> {
    let account = &launch.account;
    let state = engine.store.load()?;
    let mut command = Command::new(claude_binary());
    command.args(claude_args);
    for name in AUTH_OVERRIDE_ENV {
        command.env_remove(name);
    }
    command.env_remove("CLAUDE_CONFIG_DIR");
    command.env("CX_ACCOUNT", account.id.to_string());

    let how = match account.kind {
        Kind::ApiKey | Kind::SetupToken => {
            let secret = secrets::get(&account.secret_name())?
                .with_context(|| format!("{} has no stored key", account.display()))?;
            let variable = if account.kind == Kind::ApiKey {
                "ANTHROPIC_API_KEY"
            } else {
                "CLAUDE_CODE_OAUTH_TOKEN"
            };
            command.env(variable, secret.trim());
            "key"
        }
        Kind::Oauth => {
            let live = engine.identify_live(true)?;
            if live.account == Some(account.id) {
                // Already the default login: a second copy in a profile
                // would only be one more place for the token to go stale.
                engine.ensure_fresh(account.id, &live, false)?;
                "default login"
            } else {
                let profile = prepare_profile(account, &state)?;
                engine.ensure_fresh(account.id, &Live::default(), false)?;
                command.env("CLAUDE_CONFIG_DIR", profile.config_dir_value());
                "own session"
            }
        }
    };
    ui::note(&format!(
        "cx: Claude Code as {} ({how}; {})",
        account.display(),
        launch.why
    ));
    engine.record_session(account.id)?;
    let error = command.exec();
    bail!("could not start `{}`: {error}", claude_binary())
}
