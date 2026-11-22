//! Plain-terminal rendering for the one-shot commands.

use crate::engine::Snapshot;
use crate::store::{Account, Kind, UsageRecord, Window};
use crate::util::human_duration;
use std::io::IsTerminal;
use std::sync::OnceLock;

/// Whether output is coloured: stdout is a terminal and `NO_COLOR` is unset.
/// Decided once per process.
pub fn color() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

fn paint(text: &str, code: &str) -> String {
    if color() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// `text` wrapped in the ANSI codes for dimmed, or unchanged when [`color`] is off.
pub fn dim(text: &str) -> String {
    paint(text, "2")
}
/// `text` wrapped in the ANSI codes for bold, or unchanged when [`color`] is off.
pub fn bold(text: &str) -> String {
    paint(text, "1")
}
/// `text` wrapped in the ANSI codes for green, or unchanged when [`color`] is off.
pub fn green(text: &str) -> String {
    paint(text, "32")
}
/// `text` wrapped in the ANSI codes for yellow, or unchanged when [`color`] is off.
pub fn yellow(text: &str) -> String {
    paint(text, "33")
}
/// `text` wrapped in the ANSI codes for red, or unchanged when [`color`] is off.
pub fn red(text: &str) -> String {
    paint(text, "31")
}

/// Print a success line to stdout, after a green tick.
pub fn ok(message: &str) {
    println!("{} {message}", green("✓"));
}
/// Print a warning line to stderr, after a yellow `!`.
pub fn warn(message: &str) {
    eprintln!("{} {message}", yellow("!"));
}
/// Print a dimmed side remark to stderr, so it stays out of piped output.
pub fn note(message: &str) {
    eprintln!("{}", dim(message));
}

/// Usage level for a percentage against its switch line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// More than 25 points below the line. Shown green.
    Calm,
    /// Within 25 points of the line. Shown yellow.
    Busy,
    /// At or over the line. Shown red.
    Over,
}

/// Grade a usage percentage `pct` against its switch line `line`, both on a
/// 0 to 100 scale.
pub fn level(pct: f64, line: f64) -> Level {
    if pct >= line {
        Level::Over
    } else if pct >= line - 25.0 {
        Level::Busy
    } else {
        Level::Calm
    }
}

/// How many of a bar's `width` cells to fill for `pct` percent, rounded to
/// the nearest cell. `pct` is clamped to 0 to 100.
pub fn bar_cells(pct: f64, width: usize) -> usize {
    ((pct.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize
}

fn bar(pct: f64, line: f64, width: usize) -> String {
    let filled = bar_cells(pct, width);
    let body = "█".repeat(filled);
    let rest = "░".repeat(width - filled);
    let body = match level(pct, line) {
        Level::Calm => green(&body),
        Level::Busy => yellow(&body),
        Level::Over => red(&body),
    };
    format!("{body}{}", dim(&rest))
}

fn pad(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len > width {
        let cut: String = text.chars().take(width.saturating_sub(1)).collect();
        format!("{cut}…")
    } else {
        format!("{text}{}", " ".repeat(width - len))
    }
}

/// A fixed-width cell: bar, percent, time to reset.
fn window_cell(window: Option<&Window>, line: f64, now_ms: i64) -> String {
    const WIDTH: usize = 10;
    let Some(window) = window else {
        return dim(&pad("no limit", WIDTH + 14));
    };
    let Some(pct) = window.pct_at(now_ms) else {
        return dim(&pad("unknown", WIDTH + 14));
    };
    let reset = window
        .resets_in_s(now_ms)
        .filter(|s| *s > 0 && pct > 0.0)
        .map(human_duration)
        .unwrap_or_default();
    format!("{} {:>3.0}% {}", bar(pct, line, WIDTH), pct, dim(&pad(&reset, 8)))
}

/// The name to show for an account: `alias (label)` when it has an alias,
/// otherwise its label.
pub fn account_name(account: &Account) -> String {
    match &account.alias {
        Some(alias) => format!("{alias} ({})", account.label),
        None => account.label.clone(),
    }
}

/// A few words on anything about the account that needs saying.
pub fn status_text(account: &Account, record: Option<&UsageRecord>, sessions: usize, now_ms: i64) -> (String, Level) {
    if account.needs_login {
        return ("needs sign-in".into(), Level::Over);
    }
    let mut parts = Vec::new();
    let mut level = Level::Calm;
    if account.disabled {
        parts.push("disabled".to_string());
        level = Level::Busy;
    }
    if sessions > 0 {
        parts.push(format!("{sessions} session{}", if sessions == 1 { "" } else { "s" }));
    }
    match account.kind {
        Kind::ApiKey => parts.push("billed per token".into()),
        Kind::SetupToken => parts.push("setup token, no usage readings".into()),
        Kind::Oauth => {
            if let Some(record) = record {
                if let Some(error) = &record.error {
                    parts.push(error.clone());
                    level = Level::Busy;
                }
                if record.usage.is_some() {
                    let age = (now_ms - record.fetched_ms) / 1000;
                    if age > 600 {
                        parts.push(format!("read {} ago", human_duration(age)));
                    }
                }
            }
        }
    }
    (parts.join(" · "), level)
}

/// The table `cx list` prints: one row per account with its plan, a bar for
/// each usage window and its status, and a marker on the default login.
/// With no accounts it returns a hint on how to add one. The text ends with
/// a newline and carries ANSI colour when [`color`] is on.
pub fn render_list(snapshot: &Snapshot) -> String {
    let state = &snapshot.state;
    let now = snapshot.now_ms;
    let mut out = String::new();
    if state.accounts.is_empty() {
        return format!(
            "No accounts yet.\n\n  {}   sign in to an account in the browser\n  {}     take over the login Claude Code is using now\n  {}  bring accounts over from claude-swap\n",
            bold("cx login"),
            bold("cx add"),
            bold("cx import-cswap")
        );
    }
    let name_width = state
        .accounts
        .values()
        .map(|a| account_name(a).chars().count())
        .max()
        .unwrap_or(10)
        .clamp(10, 38);
    let models = state.model_names();
    let mut header = format!(
        "     {} {} {} {}",
        pad("ACCOUNT", name_width),
        pad("PLAN", 9),
        pad("5-HOUR", 25),
        pad("WEEK", 25)
    );
    for model in &models {
        header.push_str(&format!(" {}", pad(&format!("{} WEEK", model.to_uppercase()), 25)));
    }
    out.push_str(&dim(header.trim_end()));
    out.push('\n');
    for account in state.accounts.values() {
        let active = snapshot.live == Some(account.id);
        let marker = if active { green("●") } else { " ".into() };
        let record = state.usage.get(&account.id);
        let usage = record.and_then(|r| r.usage.as_ref());
        let name = pad(&account_name(account), name_width);
        let name = if active {
            bold(&name)
        } else if account.disabled {
            dim(&name)
        } else {
            name
        };
        let plan = pad(
            account.plan.as_deref().unwrap_or(match account.kind {
                Kind::ApiKey => "API key",
                Kind::SetupToken => "token",
                Kind::Oauth => "",
            }),
            9,
        );
        let blank = " ".repeat(24);
        let mut cells = match (account.kind, usage) {
            (Kind::Oauth, Some(u)) => format!(
                "{} {}",
                window_cell(u.five_hour.as_ref(), state.settings.threshold_5h, now),
                window_cell(u.seven_day.as_ref(), state.settings.threshold_7d, now),
            ),
            _ => format!("{blank} {blank}"),
        };
        for model in &models {
            cells.push(' ');
            match usage.and_then(|u| u.model(model)) {
                Some(window) => cells.push_str(&window_cell(Some(window), state.settings.threshold_7d, now)),
                None => cells.push_str(&blank),
            }
        }
        let sessions = snapshot.sessions.get(&account.id).copied().unwrap_or(0);
        let (mut status, level) = status_text(account, record, sessions, now);
        if account.needs_login {
            status.push_str(&format!(": cx login {}", account.id));
        }
        let status = match level {
            Level::Over => red(&status),
            Level::Busy => yellow(&status),
            Level::Calm => dim(&status),
        };
        out.push_str(format!("{marker} {:>2} {name} {} {cells} {status}", account.id, dim(&plan)).trim_end());
        out.push('\n');
        if let Some(u) = usage
            && let Some(extra) = &u.extra
        {
            out.push_str(&dim(&format!(
                "       extra usage {:.2} of {:.2} {}\n",
                extra.used, extra.limit, extra.currency
            )));
        }
    }
    out.push('\n');
    let active = match snapshot.live.and_then(|id| state.accounts.get(&id)) {
        Some(a) => format!("default login {}", bold(&a.display())),
        None if snapshot.live_unmanaged => yellow("default login is not managed by cx (run `cx add`)"),
        None => yellow("Claude Code is not signed in"),
    };
    let best = snapshot
        .verdicts
        .iter()
        .find(|v| v.eligible && v.room.as_ref().is_some_and(|r| r.min > 0.0))
        .and_then(|v| state.accounts.get(&v.id))
        .map(|a| format!("best now {}", a.display()))
        .unwrap_or_else(|| "no account has room".into());
    out.push_str(&format!(
        "{active}  {}  {best}  {}  strategy {}\n",
        dim("·"),
        dim("·"),
        state.settings.strategy
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_fills_in_proportion() {
        assert_eq!(bar_cells(0.0, 10), 0);
        assert_eq!(bar_cells(50.0, 10), 5);
        assert_eq!(bar_cells(100.0, 10), 10);
        assert_eq!(bar_cells(250.0, 10), 10);
    }

    #[test]
    fn levels_are_relative_to_the_switch_line() {
        assert_eq!(level(10.0, 90.0), Level::Calm);
        assert_eq!(level(70.0, 90.0), Level::Busy);
        assert_eq!(level(90.0, 90.0), Level::Over);
    }

    #[test]
    fn pad_truncates_with_an_ellipsis() {
        assert_eq!(pad("abc", 5), "abc  ");
        assert_eq!(pad("abcde", 5), "abcde");
        assert_eq!(pad("abcdefgh", 5), "abcd…");
    }
}
