//! `cx plugin`: the cx plugin for Claude Code, installed with one command.
//!
//! The plugin lives in the Protoworks marketplace, a public GitHub
//! repository. Installing it by hand takes two `claude` commands and the
//! names of the marketplace and the plugin. cx runs them: everything here
//! goes through Claude Code's own `claude plugin` command, so the plugin is
//! installed, updated and removed exactly as if the user had typed it.

use crate::run::claude_binary;
use crate::util::{self, run_bounded};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::process::Command;
use std::time::Duration;

/// The GitHub repository that holds the Protoworks marketplace.
pub const MARKETPLACE_REPO: &str = "protoworks-dev/claude-plugins";
/// The marketplace's name, as its `marketplace.json` declares it.
pub const MARKETPLACE: &str = "protoworks";
/// The plugin's id: its name and the marketplace it comes from.
pub const PLUGIN: &str = "cx@protoworks";

/// Adding a marketplace clones a repository, so the limit is generous.
const TIMEOUT: Duration = Duration::from_secs(180);

/// What Claude Code has of the plugin right now.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Status {
    /// Whether Claude Code knows the Protoworks marketplace.
    pub marketplace: bool,
    /// The installed version, or `None` when the plugin is not installed.
    pub version: Option<String>,
    /// Whether the installed plugin is switched on.
    pub enabled: bool,
}

fn claude(args: &[&str]) -> Result<util::Output> {
    let mut command = Command::new(claude_binary());
    // The plugin goes into the default home. Profiles share it by symlink,
    // so a `cx plugin` typed inside a `cx run` session must not install
    // into that one profile.
    command.args(args).env_remove("CLAUDE_CONFIG_DIR");
    run_bounded(&mut command, None, TIMEOUT).with_context(|| {
        format!(
            "`{}` could not be run. Install Claude Code or set CX_CLAUDE_BIN",
            claude_binary()
        )
    })
}

fn listing(args: &[&str]) -> Result<Vec<Value>> {
    let out = claude(args)?;
    if out.code != 0 {
        bail!(
            "`claude {}` failed (exit {}): {}",
            args.join(" "),
            out.code,
            out.stderr.trim()
        );
    }
    let value: Value = serde_json::from_str(out.stdout.trim()).with_context(|| {
        format!(
            "`claude {}` did not print JSON; Claude Code may be too old",
            args.join(" ")
        )
    })?;
    Ok(value.as_array().cloned().unwrap_or_default())
}

/// Read the plugin's state from two listings Claude Code prints as JSON.
fn status_from(marketplaces: &[Value], plugins: &[Value]) -> Status {
    let installed = plugins.iter().find(|p| p["id"] == PLUGIN);
    Status {
        marketplace: marketplaces.iter().any(|m| m["name"] == MARKETPLACE),
        version: installed.map(|p| p["version"].as_str().unwrap_or("unknown").to_string()),
        enabled: installed.is_some_and(|p| p["enabled"].as_bool().unwrap_or(true)),
    }
}

/// Ask Claude Code which marketplaces and plugins it has. Runs `claude`
/// twice; makes no network call.
pub fn status() -> Result<Status> {
    Ok(status_from(
        &listing(&["plugin", "marketplace", "list", "--json"])?,
        &listing(&["plugin", "list", "--json"])?,
    ))
}

/// Run one `claude plugin` step and fail with its own words when it fails.
fn step(what: &str, args: &[&str]) -> Result<()> {
    let out = claude(args)?;
    if out.code != 0 {
        let said = if out.stderr.trim().is_empty() {
            out.stdout.trim()
        } else {
            out.stderr.trim()
        };
        bail!("{what} failed (`claude {}`, exit {}): {said}", args.join(" "), out.code);
    }
    Ok(())
}

/// Install the plugin, or bring an installed one up to date. Adds the
/// marketplace when Claude Code does not know it and refreshes it when it
/// does, then installs, updates or switches on the plugin. Reaches GitHub.
/// Returns one line per thing done, for the caller to print.
pub fn install() -> Result<Vec<String>> {
    let before = status()?;
    let mut done = Vec::new();
    if before.marketplace {
        step(
            "refreshing the Protoworks marketplace",
            &["plugin", "marketplace", "update", MARKETPLACE],
        )?;
        done.push("refreshed the Protoworks marketplace".to_string());
    } else {
        step(
            "adding the Protoworks marketplace",
            &["plugin", "marketplace", "add", MARKETPLACE_REPO],
        )?;
        done.push(format!(
            "added the Protoworks marketplace (github.com/{MARKETPLACE_REPO})"
        ));
    }
    match &before.version {
        None => step("installing the plugin", &["plugin", "install", PLUGIN])?,
        Some(_) => {
            step("updating the plugin", &["plugin", "update", PLUGIN])?;
            if !before.enabled {
                step("switching the plugin on", &["plugin", "enable", PLUGIN])?;
            }
        }
    }
    let after = status()?;
    let version = after
        .version
        .context("Claude Code reported success but does not list the plugin")?;
    done.push(match before.version {
        None => format!("installed {PLUGIN} {version}"),
        Some(old) if old == version => format!("{PLUGIN} {version} is the newest"),
        Some(old) => format!("updated {PLUGIN} from {old} to {version}"),
    });
    Ok(done)
}

/// Uninstall the plugin. The marketplace stays, since other Protoworks
/// plugins may come from it. `Ok(false)` when it was not installed.
pub fn remove() -> Result<bool> {
    if status()?.version.is_none() {
        return Ok(false);
    }
    step("removing the plugin", &["plugin", "uninstall", PLUGIN])?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_is_read_from_claude_codes_listings() {
        let marketplaces = [json!({"name": "claude-plugins-official", "source": "github"})];
        let plugins = [json!({"id": "cx@elsewhere", "version": "9.9.9", "enabled": true})];
        assert_eq!(status_from(&marketplaces, &plugins), Status::default());

        let marketplaces = [json!({"name": "protoworks", "source": "github", "repo": MARKETPLACE_REPO})];
        let plugins = [
            json!({"id": "other@protoworks", "version": "1.0.0", "enabled": true}),
            json!({"id": "cx@protoworks", "version": "0.1.0", "enabled": false}),
        ];
        assert_eq!(
            status_from(&marketplaces, &plugins),
            Status {
                marketplace: true,
                version: Some("0.1.0".into()),
                enabled: false
            }
        );
    }
}
