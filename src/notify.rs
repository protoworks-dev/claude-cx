//! Desktop notifications, for the things `cx auto` does while nobody is
//! watching. macOS only; elsewhere this does nothing.

use crate::engine::Snapshot;
use crate::store::Kind;
use crate::util;
use std::collections::BTreeSet;
use std::io::Write;
use std::process::Command;
use std::time::Duration;

fn quote(text: &str) -> String {
    // AppleScript string literals cannot hold a raw line break.
    let flat = text.replace(['\n', '\r'], " ");
    format!("\"{}\"", flat.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Show a desktop notification through `osascript`, waiting at most 5
/// seconds for it. Failures are ignored. When `CX_NOTIFY_LOG` is set, the
/// line `title: body` is appended to that file and nothing is shown.
pub fn send(title: &str, body: &str) {
    // Tests point this at a file so no real notification is shown.
    if let Some(path) = std::env::var_os("CX_NOTIFY_LOG") {
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{title}: {body}");
        }
        return;
    }
    if !cfg!(target_os = "macos") {
        return;
    }
    let script = format!("display notification {} with title {}", quote(body), quote(title));
    let _ = util::run_bounded(
        Command::new("/usr/bin/osascript").arg("-e").arg(script),
        None,
        Duration::from_secs(5),
    );
}

/// Remembers what was already reported, so each event notifies once.
#[derive(Default)]
pub struct Watch {
    needs_login: Option<BTreeSet<u32>>,
    exhausted: bool,
}

impl Watch {
    /// Look at one pass of automatic switching and notify about what is new.
    /// Returns the lines it sent, for a log.
    pub fn observe(&mut self, snapshot: &Snapshot, switched_to: Option<(u32, &str)>) -> Vec<String> {
        let state = &snapshot.state;
        let mut lines = Vec::new();
        let name = |id: u32| {
            state
                .accounts
                .get(&id)
                .map(|a| a.display())
                .unwrap_or_else(|| format!("#{id}"))
        };
        if let Some((id, reason)) = switched_to {
            lines.push(format!("Now on {} ({reason})", name(id)));
        }
        let needing: BTreeSet<u32> = state
            .accounts
            .values()
            .filter(|a| a.needs_login)
            .map(|a| a.id)
            .collect();
        // The first pass sets the baseline: what already needed a sign-in
        // when cx started is on the dashboard, not news.
        if let Some(before) = &self.needs_login {
            for id in needing.difference(before) {
                lines.push(format!("{} needs a new sign-in: cx login {id}", name(*id)));
            }
        }
        self.needs_login = Some(needing);
        let subscriptions = state.accounts.values().any(|a| a.kind == Kind::Oauth && !a.disabled);
        let room = snapshot
            .verdicts
            .iter()
            .any(|v| v.eligible && v.room.as_ref().is_none_or(|r| !r.at_limit));
        let exhausted = subscriptions && !room;
        if exhausted && !self.exhausted {
            let soonest = snapshot
                .verdicts
                .iter()
                .filter_map(|v| v.room.as_ref().and_then(|r| r.resets_in_s).filter(|s| *s > 0))
                .min();
            lines.push(match soonest {
                Some(s) => format!(
                    "Every account is at its limit. The first frees up in {}.",
                    util::human_duration(s)
                ),
                None => "No account has room right now.".to_string(),
            });
        }
        self.exhausted = exhausted;
        if state.settings.notify {
            for line in &lines {
                send("cx", line);
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn quotes_survive_applescript() {
        assert_eq!(super::quote(r#"a "b" \c"#), r#""a \"b\" \\c""#);
        assert_eq!(super::quote("two\nlines"), "\"two lines\"");
    }
}
