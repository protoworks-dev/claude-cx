//! The dashboard: `cx` with no arguments. One screen does everything.
//!
//! Top to bottom: who is signed in and how the fleet stands; the accounts,
//! with limits that apply to all models kept apart from the ones that apply
//! to a single model; the selected account's limits and what used them
//! (read from Claude Code's transcripts); then the rotation for each kind
//! of work, the API keys, and what cx did. Settings, key entry and help are
//! pop-ups over this screen. Rows and buttons take mouse clicks.
//!
//! The screen thread only draws and reads input. The network, the Keychain
//! and locks are on a worker thread; the transcripts are read on another.

use crate::accounts;
use crate::activity::{self, Activity, Summary, Tokens};
use crate::claude::ClaudeHome;
use crate::engine::{Engine, Snapshot};
use crate::history::{self, Forecast, History, Key};
use crate::notify;
use crate::rank::{self, Decision, Verdict};
use crate::store::{self, Account, Kind, Settings, Usage, Window};
use crate::ui::{self, Level};
use crate::util::{human_duration, now_ms};
use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode,
    KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Axis, Bar, BarChart, BarGroup, Block, BorderType, Cell, Chart, Clear, Dataset, GraphType, LineGauge, Padding,
    Paragraph, Row, Table, TableState, Tabs, Wrap,
};
use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_secs(5);
const HOUR_MS: i64 = 3_600_000;

// ---- colour ---------------------------------------------------------------

/// Terminals that say they do 24-bit colour get it; the rest get the
/// nearest of the 256.
fn rich() -> bool {
    static RICH: OnceLock<bool> = OnceLock::new();
    *RICH.get_or_init(|| std::env::var("COLORTERM").is_ok_and(|v| v.contains("truecolor") || v.contains("24bit")))
}

fn rgb(r: u8, g: u8, b: u8) -> Color {
    if rich() {
        Color::Rgb(r, g, b)
    } else {
        let cube = |v: u8| (v as u16 * 5 + 127) / 255;
        Color::Indexed((16 + 36 * cube(r) + 6 * cube(g) + cube(b)) as u8)
    }
}

fn accent() -> Color {
    rgb(125, 165, 255)
}
/// The colour of everything that is about one model only.
fn violet() -> Color {
    rgb(190, 150, 255)
}
fn good() -> Color {
    rgb(95, 215, 150)
}
fn warn() -> Color {
    rgb(245, 195, 90)
}
fn bad() -> Color {
    rgb(255, 105, 115)
}
fn muted() -> Color {
    rgb(135, 143, 165)
}
fn faint() -> Color {
    rgb(78, 86, 108)
}
fn selected_bg() -> Color {
    rgb(38, 46, 66)
}

fn dim() -> Style {
    Style::new().fg(muted())
}
fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}
fn heading(text: &str, color: Color) -> Line<'static> {
    Line::from(Span::styled(
        text.to_uppercase(),
        Style::new().fg(color).add_modifier(Modifier::BOLD),
    ))
}

// ---- small drawing pieces -------------------------------------------------

/// Cells filled for `fraction` of `width`. Anything above zero shows one.
fn bar_cells(fraction: f64, width: usize) -> usize {
    let cells = (fraction.clamp(0.0, 1.0) * width as f64).round() as usize;
    if fraction > 0.0 { cells.max(1) } else { cells }
}

/// A thin bar in one colour on a faint line. It is a line, not a block, so
/// bars in neighbouring rows never run together.
fn bar(fraction: f64, width: usize, color: Color) -> Vec<Span<'static>> {
    let filled = bar_cells(fraction, width);
    vec![
        Span::styled("━".repeat(filled), Style::new().fg(color)),
        Span::styled("─".repeat(width - filled), Style::new().fg(faint())),
    ]
}

/// The colour of a limit at `pct` against its switch line. A limit on all
/// models runs green, amber, red; a single model's runs violet, amber, red,
/// so the two kinds are never mistaken for each other.
fn limit_color(pct: f64, line: f64, one_model: bool) -> Color {
    match ui::level(pct, line) {
        Level::Calm if one_model => violet(),
        Level::Calm => good(),
        Level::Busy => warn(),
        Level::Over => bad(),
    }
}

/// One row of a trend: a column per value, taller is fuller.
fn trend(values: &[Option<f64>], line: f64) -> Line<'static> {
    Line::from(
        values
            .iter()
            .map(|value| match value {
                None => Span::styled("·", Style::new().fg(faint())),
                Some(pct) => {
                    let step = ((pct.clamp(0.0, 100.0) / 100.0) * 8.0).round() as usize;
                    let symbol = ["▁", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"][step];
                    Span::styled(symbol, Style::new().fg(limit_color(*pct, line, false)))
                }
            })
            .collect::<Vec<_>>(),
    )
}

fn panel(title: impl Into<String>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(faint()))
        .title(Span::styled(
            format!(" {} ", title.into()),
            Style::new().fg(muted()).add_modifier(Modifier::BOLD),
        ))
}

/// 1234567 reads "1.2M".
fn compact(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        1_000_000..=999_999_999 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.2}B", n as f64 / 1e9),
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", compact(n), if n == 1 { one } else { many })
}

// ---- what the numbers say -------------------------------------------------

/// One limit of one account, with the line it is judged against.
struct Limit<'a> {
    key: Key,
    window: &'a Window,
    line: f64,
}

impl Limit<'_> {
    fn one_model(&self) -> bool {
        matches!(self.key, Key::Model(_))
    }
}

fn limits_of<'a>(usage: &'a Usage, settings: &Settings) -> Vec<Limit<'a>> {
    let mut limits = Vec::new();
    if let Some(window) = &usage.five_hour {
        limits.push(Limit {
            key: Key::FiveHour,
            window,
            line: settings.threshold_5h,
        });
    }
    if let Some(window) = &usage.seven_day {
        limits.push(Limit {
            key: Key::Week,
            window,
            line: settings.threshold_7d,
        });
    }
    for model in &usage.models {
        limits.push(Limit {
            key: Key::Model(model.name.clone()),
            window: &model.window,
            line: settings.threshold_7d,
        });
    }
    limits
}

fn forecast_of(history: &History, account: u32, limit: &Limit, now: i64) -> Option<Forecast> {
    let pct = limit.window.pct_at(now)?;
    // The burn rate looks back an hour for the 5-hour window, six for a weekly one.
    let lookback = match limit.key {
        Key::FiveHour => HOUR_MS,
        _ => 6 * HOUR_MS,
    };
    let series = history.series(account, &limit.key, now - lookback);
    let rate = history::burn_rate(&series, now, lookback)?;
    Some(history::forecast(pct, rate, limit.line, limit.window.resets_in_s(now)))
}

/// The forecast in a few words, and how loudly to say it.
fn forecast_text(forecast: &Forecast) -> (String, Color) {
    if forecast.rate_per_h < 0.05 {
        return ("not rising".into(), muted());
    }
    let rate = if forecast.rate_per_h >= 10.0 {
        format!("rising {:.0}% an hour", forecast.rate_per_h)
    } else {
        format!("rising {:.1}% an hour", forecast.rate_per_h)
    };
    match forecast.to_line_s {
        Some(0) => (format!("{rate}, past the switch line"), bad()),
        Some(s) if !forecast.reset_first => (
            format!("{rate}, switch line in {}", human_duration(s)),
            if s < 3600 { bad() } else { warn() },
        ),
        _ => (format!("{rate}, resets before the switch line"), good()),
    }
}

/// How one kind of work stands on an account. Later is worse.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Standing {
    Ready,
    /// Ten points or fewer under the switch line.
    Low,
    /// Past the switch line; the server still answers.
    Over,
    /// Used up; the server refuses until the reset.
    Full,
}

fn standing(pct: f64, line: f64) -> Standing {
    if pct >= 100.0 {
        Standing::Full
    } else if pct >= line {
        Standing::Over
    } else if pct >= line - 10.0 {
        Standing::Low
    } else {
        Standing::Ready
    }
}

/// The tightest limit of one kind: its standing, its name and the seconds
/// to its reset.
fn worst(limits: &[Limit], one_model: bool, now: i64) -> Option<(Standing, String, Option<i64>)> {
    limits
        .iter()
        .filter(|l| l.one_model() == one_model)
        .filter_map(|l| {
            let pct = l.window.pct_at(now)?;
            let name = match &l.key {
                Key::FiveHour => "5-hour".to_string(),
                Key::Week => "week".to_string(),
                Key::Model(name) => name.clone(),
            };
            Some((
                standing(pct, l.line),
                name,
                l.window.resets_in_s(now).filter(|s| *s > 0),
                l.line - pct,
            ))
        })
        .min_by(|a, b| a.3.total_cmp(&b.3))
        .map(|(standing, name, reset, _)| (standing, name, reset))
}

/// A weekly plan's size next to a Pro plan, for the fleet total. An
/// estimate: the plans' allowances are not exact multiples.
fn plan_weight(plan: Option<&str>) -> f64 {
    match plan {
        Some(p) if p.contains("20x") => 20.0,
        Some(p) if p.contains("5x") => 5.0,
        Some(p) if p.contains("Pro") => 1.0,
        _ => 5.0,
    }
}

#[derive(Default, Debug, PartialEq)]
struct Fleet {
    ready: usize,
    over: usize,
    needs_login: usize,
    disabled: usize,
    sessions: usize,
    /// Percent of the week used across the subscriptions in rotation,
    /// weighted by plan.
    week_used: Option<f64>,
    /// The soonest weekly reset: seconds and the account.
    week_reset: Option<(i64, u32)>,
}

fn fleet_of(snapshot: &Snapshot) -> Fleet {
    let state = &snapshot.state;
    let now = snapshot.now_ms;
    let mut fleet = Fleet {
        sessions: snapshot.sessions.values().sum(),
        ..Fleet::default()
    };
    let (mut week, mut weight_sum) = (0.0, 0.0);
    for account in state.accounts.values().filter(|a| a.kind == Kind::Oauth) {
        if account.needs_login {
            fleet.needs_login += 1;
            continue;
        }
        if account.disabled {
            fleet.disabled += 1;
            continue;
        }
        let past = snapshot
            .verdict(account.id)
            .and_then(|v| v.room.as_ref())
            .is_some_and(|r| r.at_limit || r.min <= 0.0);
        if past {
            fleet.over += 1;
        } else {
            fleet.ready += 1;
        }
        let Some(window) = state
            .usage
            .get(&account.id)
            .and_then(|r| r.usage.as_ref())
            .and_then(|u| u.seven_day.as_ref())
        else {
            continue;
        };
        if let Some(pct) = window.pct_at(now) {
            let weight = plan_weight(account.plan.as_deref());
            week += pct * weight;
            weight_sum += weight;
            if let Some(reset) = window.resets_in_s(now).filter(|s| *s > 0 && pct > 0.0)
                && fleet.week_reset.is_none_or(|(s, _)| reset < s)
            {
                fleet.week_reset = Some((reset, account.id));
            }
        }
    }
    fleet.week_used = (weight_sum > 0.0).then(|| week / weight_sum);
    fleet
}

/// The accounts in the order cx would use them for one kind of work:
/// `model` names the single-model limit that counts, `None` for work on
/// models that have none.
fn lane(snapshot: &Snapshot, model: Option<&str>) -> Vec<Verdict> {
    let mut state = snapshot.state.clone();
    state.settings.model = model.map(String::from);
    rank::evaluate(&state, &snapshot.sessions, state.settings.strategy, snapshot.now_ms)
}

fn usable(verdict: &Verdict) -> bool {
    verdict.eligible && verdict.room.as_ref().is_none_or(|r| !r.at_limit && r.min > 0.0)
}

// ---- the worker -----------------------------------------------------------

enum Job {
    Refresh { force: bool },
    Switch(u32),
    SwitchBest,
    RefreshToken(u32),
    ToggleDisabled(u32),
    Priority(u32, i32),
    Remove(u32),
    SetAuto(bool),
    SetSetting(String, String),
    Alias(u32, Option<String>),
    AddKey { label: String, key: String },
    Quit,
}

enum Update {
    Snapshot(Box<Snapshot>),
    History(Box<History>),
    Activity(Arc<Activity>),
    Log(String),
    Working(Option<String>),
}

fn name_of(snapshot: &Snapshot, id: u32) -> String {
    snapshot
        .state
        .accounts
        .get(&id)
        .map(|a| a.display())
        .unwrap_or_else(|| format!("#{id}"))
}

fn worker(engine: Engine, jobs: Receiver<Job>, updates: Sender<Update>) {
    let mut auto = false;
    let mut last_auto_line = String::new();
    let mut watch = notify::Watch::default();
    let send_snapshot = |force: bool| match engine.snapshot(force, true) {
        Ok(s) => {
            let _ = updates.send(Update::Snapshot(Box::new(s)));
            let _ = updates.send(Update::History(Box::new(engine.history())));
        }
        Err(e) => {
            let _ = updates.send(Update::Log(format!("reading the accounts failed: {e:#}")));
        }
    };
    let log = |text: String| {
        let _ = updates.send(Update::Log(text));
    };
    let working = |what: Option<&str>| {
        let _ = updates.send(Update::Working(what.map(String::from)));
    };
    // What is already known appears at once; the network pass follows.
    if let Ok(s) = engine.snapshot(false, false) {
        let _ = updates.send(Update::Snapshot(Box::new(s)));
        let _ = updates.send(Update::History(Box::new(engine.history())));
    }
    working(Some("reading usage"));
    send_snapshot(false);
    working(None);
    loop {
        match jobs.recv_timeout(TICK) {
            Ok(Job::Quit) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(Job::Refresh { force }) => {
                working(Some("reading usage"));
                send_snapshot(force);
            }
            Ok(Job::Switch(id)) => {
                working(Some("switching"));
                match engine.switch_to(id) {
                    Ok(outcome) => {
                        let state = engine.store.load().ok();
                        let name = state
                            .and_then(|s| s.accounts.get(&id).map(|a| a.display()))
                            .unwrap_or_default();
                        log(if outcome.changed {
                            format!("default login is now {name}")
                        } else {
                            format!("{name} is already the default login")
                        });
                        if let Some(stash) = outcome.stashed {
                            log(format!("the previous login was not managed by cx; kept as '{stash}'"));
                        }
                    }
                    Err(e) => log(format!("switch failed: {e:#}")),
                }
                send_snapshot(false);
            }
            Ok(Job::SwitchBest) => {
                working(Some("choosing"));
                match engine.snapshot(false, true) {
                    Ok(snapshot) => match engine.pick(None, &snapshot) {
                        Some(best) => match engine.switch_to(best.id) {
                            Ok(_) => log(format!(
                                "default login is now {} ({})",
                                name_of(&snapshot, best.id),
                                best.note
                            )),
                            Err(e) => log(format!("switch failed: {e:#}")),
                        },
                        None => log("no account has room right now".into()),
                    },
                    Err(e) => log(format!("choosing failed: {e:#}")),
                }
                send_snapshot(false);
            }
            Ok(Job::RefreshToken(id)) => {
                working(Some("renewing the token"));
                let result = engine
                    .identify_live(true)
                    .and_then(|live| engine.ensure_fresh(id, &live, true));
                match result {
                    Ok(oauth) => log(format!(
                        "#{id}: token renewed, valid for {}",
                        human_duration((oauth.expires_ms() - now_ms()) / 1000)
                    )),
                    Err(e) => log(format!("renewing failed: {e:#}")),
                }
                send_snapshot(false);
            }
            Ok(Job::ToggleDisabled(id)) => {
                let result = engine.store.update(|state| {
                    let account = state
                        .accounts
                        .get_mut(&id)
                        .ok_or_else(|| anyhow::anyhow!("no account #{id}"))?;
                    account.disabled = !account.disabled;
                    Ok((account.display(), account.disabled))
                });
                match result {
                    Ok((name, true)) => log(format!("{name} is off: never chosen automatically")),
                    Ok((name, false)) => log(format!("{name} is back on")),
                    Err(e) => log(format!("not changed: {e:#}")),
                }
                send_snapshot(false);
            }
            Ok(Job::Priority(id, delta)) => {
                let _ = engine.store.update(|state| {
                    if let Some(account) = state.accounts.get_mut(&id) {
                        account.priority = (account.priority + delta).clamp(-9, 9);
                    }
                    Ok(())
                });
                send_snapshot(false);
            }
            Ok(Job::Remove(id)) => {
                match engine.remove(id) {
                    Ok(account) => log(format!("removed {}", account.display())),
                    Err(e) => log(format!("remove failed: {e:#}")),
                }
                send_snapshot(false);
            }
            Ok(Job::SetAuto(on)) => {
                auto = on;
                last_auto_line.clear();
                log(if on {
                    "automatic switching is on while this dashboard is open".into()
                } else {
                    "automatic switching is off".into()
                });
            }
            Ok(Job::SetSetting(key, value)) => {
                match engine.store.update(|state| state.settings.set(&key, &value)) {
                    Ok(()) => log(format!("setting changed: {} is {value}", setting_title(&key))),
                    Err(e) => log(format!("not changed: {e:#}")),
                }
                send_snapshot(false);
            }
            Ok(Job::Alias(id, name)) => {
                match engine.store.update(|state| state.set_alias(id, name.as_deref())) {
                    Ok(display) => log(format!("renamed: {display}")),
                    Err(e) => log(format!("not renamed: {e:#}")),
                }
                send_snapshot(false);
            }
            Ok(Job::AddKey { label, key }) => {
                working(Some("storing the key"));
                let label = Some(label.trim()).filter(|l| !l.is_empty());
                match accounts::add_key(&engine, &key, label) {
                    Ok(account) => {
                        // A stored key is there to be used: turn the fallback on.
                        let was_on = engine
                            .store
                            .update(|state| {
                                let was = state.settings.api_fallback;
                                state.settings.api_fallback = true;
                                Ok(was)
                            })
                            .unwrap_or(true);
                        log(format!(
                            "stored key {}{}",
                            account.display(),
                            if was_on {
                                ""
                            } else {
                                "; keys are now used when every subscription is at its limit"
                            }
                        ));
                    }
                    Err(e) => log(format!("key not stored: {e:#}")),
                }
                send_snapshot(false);
            }
            Err(RecvTimeoutError::Timeout) => {
                if auto {
                    match engine.auto_step(false) {
                        Ok((snapshot, decision, outcome)) => {
                            let moved = match (&decision, &outcome) {
                                (Decision::Switch { to, reason }, Some(_)) => Some((*to, reason.as_str())),
                                _ => None,
                            };
                            let told = watch.observe(&snapshot, moved);
                            let line = match (&decision, outcome) {
                                (Decision::Switch { to, reason }, Some(_)) => {
                                    format!("auto: switched to {} ({reason})", name_of(&snapshot, *to))
                                }
                                (Decision::Stay(reason), _) => format!("auto: staying ({reason})"),
                                _ => String::new(),
                            };
                            if !line.is_empty() && line != last_auto_line {
                                log(line.clone());
                                last_auto_line = line;
                            }
                            for text in told.into_iter().filter(|t| !t.starts_with("Now on")) {
                                log(text);
                            }
                        }
                        Err(e) => log(format!("auto failed: {e:#}")),
                    }
                }
                send_snapshot(false);
            }
        }
        working(None);
    }
}

/// Keeps the sums over Claude Code's transcripts up to date.
fn activity_reader(store_root: std::path::PathBuf, updates: Sender<Update>) {
    let claude = ClaudeHome::default_home().dir;
    let mut activity = Activity::load(&store_root);
    let mut first = true;
    loop {
        if activity.refresh(&claude, &store_root) || first {
            first = false;
            if updates.send(Update::Activity(Arc::new(activity.clone()))).is_err() {
                return;
            }
        }
        std::thread::sleep(Duration::from_secs(20));
    }
}

// ---- state of the screen --------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sort {
    Number,
    Room,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Accounts,
    Keys,
}

/// A pop-up over the screen. The value editors open from the settings list
/// and return to it.
#[derive(Clone, PartialEq, Debug)]
enum Modal {
    None,
    Help,
    ConfirmRemove(u32),
    /// Typing a short name for an account.
    Alias(u32, String),
    Settings,
    /// A number with up and down: the key, the value, what was typed.
    Number {
        key: &'static str,
        value: f64,
        typed: String,
    },
    /// One of a few values.
    Choice {
        key: &'static str,
        options: Vec<String>,
        at: usize,
    },
    /// A list of entries, each on or off.
    Checklist {
        key: &'static str,
        items: Vec<(String, bool)>,
        at: usize,
    },
    /// Entering an API key: the name, the key, which field has the cursor.
    KeyInput {
        label: String,
        key: String,
        on_key: bool,
        error: Option<String>,
    },
}

/// Something on screen that takes a click.
#[derive(Clone, PartialEq, Debug)]
enum Hit {
    Account(u32),
    ApiKey(u32),
    AddKey,
    Button(char),
    ListRow(usize),
    Up,
    Down,
    /// Opens the editor of one setting.
    Setting(&'static str),
    /// The breakdown's period: today only, or the last 7 days.
    Period(bool),
}

/// What the main loop does itself, because it needs the terminal or the
/// engine the screen thread holds.
enum Flow {
    Continue,
    Quit,
    SignIn { again: bool },
    OpenClaude(u32),
    TakeOver,
}

/// A breakdown worked out for the screen.
struct Shown {
    uuid: String,
    today_only: bool,
    summary: Summary,
    daily: Vec<(String, Tokens)>,
}

struct App {
    snapshot: Option<Snapshot>,
    history: History,
    activity: Option<Arc<Activity>>,
    /// The breakdown on screen: the account, whether it covers today only,
    /// and the sums. Recomputed when the account, the period or the
    /// transcripts change.
    summary: Option<Shown>,
    /// Whether closing a value editor returns to the settings list.
    back_to_settings: bool,
    today_only: bool,
    focus: Focus,
    /// The selected account, by number, so it stays put when the order moves.
    selected: Option<u32>,
    /// The selected row of the keys table: a key, or `None` for "add a key".
    selected_key: Option<u32>,
    table: TableState,
    log: VecDeque<(String, String)>,
    working: Option<String>,
    auto: bool,
    modal: Modal,
    settings_row: usize,
    started: Instant,
    sort: Sort,
    /// Where the clickable things were drawn in the last frame.
    hits: Vec<(Rect, Hit)>,
}

impl App {
    fn new() -> App {
        App {
            snapshot: None,
            history: History::default(),
            activity: None,
            summary: None,
            back_to_settings: false,
            today_only: false,
            focus: Focus::Accounts,
            selected: None,
            selected_key: None,
            table: TableState::default(),
            log: VecDeque::new(),
            working: None,
            auto: false,
            modal: Modal::None,
            settings_row: 0,
            started: Instant::now(),
            sort: Sort::Number,
            hits: Vec::new(),
        }
    }

    /// Subscription accounts in the order the table shows them.
    fn ids(&self) -> Vec<u32> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let mut ids: Vec<u32> = snapshot
            .state
            .accounts
            .values()
            .filter(|a| a.kind != Kind::ApiKey)
            .map(|a| a.id)
            .collect();
        if self.sort == Sort::Room {
            let place = |id: &u32| snapshot.verdicts.iter().position(|v| v.id == *id).unwrap_or(usize::MAX);
            ids.sort_by_key(place);
        }
        ids
    }

    fn key_ids(&self) -> Vec<u32> {
        self.snapshot
            .as_ref()
            .map(|s| {
                s.state
                    .accounts
                    .values()
                    .filter(|a| a.kind == Kind::ApiKey)
                    .map(|a| a.id)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn selected(&self) -> Option<u32> {
        let ids = self.ids();
        self.selected.filter(|id| ids.contains(id)).or(ids.first().copied())
    }

    fn selected_key(&self) -> Option<u32> {
        self.selected_key.filter(|id| self.key_ids().contains(id))
    }

    fn move_selection(&mut self, delta: i32) {
        match self.focus {
            Focus::Accounts => {
                let ids = self.ids();
                if ids.is_empty() {
                    return;
                }
                let current = self
                    .selected()
                    .and_then(|id| ids.iter().position(|i| *i == id))
                    .unwrap_or(0) as i32;
                self.selected = Some(ids[(current + delta).rem_euclid(ids.len() as i32) as usize]);
            }
            Focus::Keys => {
                // The rows are the keys, then "add a key".
                let ids = self.key_ids();
                let rows = ids.len() as i32 + 1;
                let current = self
                    .selected_key()
                    .and_then(|id| ids.iter().position(|i| *i == id))
                    .unwrap_or(ids.len()) as i32;
                let next = (current + delta).rem_euclid(rows) as usize;
                self.selected_key = ids.get(next).copied();
            }
        }
    }

    fn push_log(&mut self, text: String) {
        let stamp = chrono::Local::now().format("%H:%M:%S").to_string();
        self.log.push_front((stamp, text));
        self.log.truncate(80);
    }

    /// The breakdown for an account and its sums per day, computed once
    /// per change.
    fn summary_for(&mut self, uuid: &str) -> Option<(Summary, Vec<(String, Tokens)>)> {
        let activity = self.activity.clone()?;
        let fresh = self
            .summary
            .as_ref()
            .is_some_and(|shown| shown.uuid == uuid && shown.today_only == self.today_only);
        if !fresh {
            let since = if self.today_only {
                activity::today()
            } else {
                String::new()
            };
            self.summary = Some(Shown {
                uuid: uuid.to_string(),
                today_only: self.today_only,
                summary: activity.summary(uuid, &since),
                daily: activity.daily(uuid),
            });
        }
        self.summary
            .as_ref()
            .map(|shown| (shown.summary.clone(), shown.daily.clone()))
    }

    fn hit_at(&self, column: u16, row: u16) -> Option<Hit> {
        self.hits
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(Position::new(column, row)))
            .map(|(_, hit)| hit.clone())
    }

    fn open_key_input(&mut self) {
        self.focus = Focus::Keys;
        self.selected_key = None;
        self.modal = Modal::KeyInput {
            label: String::new(),
            key: String::new(),
            on_key: false,
            error: None,
        };
    }

    /// Open the editor of the setting chosen in the settings list.
    fn edit_setting(&mut self, jobs: &Sender<Job>) {
        let (key, _) = store::SETTING_HELP[self.settings_row.min(store::SETTING_HELP.len() - 1)];
        self.back_to_settings = true;
        self.open_editor(key, jobs);
    }

    /// Where a value editor goes when it closes.
    fn back(&self) -> Modal {
        if self.back_to_settings {
            Modal::Settings
        } else {
            Modal::None
        }
    }

    /// Open the editor that fits a setting, or flip it when it is on/off.
    fn open_editor(&mut self, key: &'static str, jobs: &Sender<Job>) {
        let Some(snapshot) = &self.snapshot else { return };
        let mut settings = snapshot.state.settings.clone();
        settings.model = snapshot.model_setting.clone();
        let current = settings.show(key).unwrap_or_default();
        if let Some(options) = Settings::choices(key, &snapshot.state.model_names()) {
            let at = options
                .iter()
                .position(|o| o.eq_ignore_ascii_case(&current))
                .unwrap_or(0);
            self.modal = Modal::Choice { key, options, at };
        } else if Settings::number_range(key).is_some() {
            self.modal = Modal::Number {
                key,
                value: current.parse().unwrap_or(0.0),
                typed: String::new(),
            };
        } else if key == "share" {
            let mut items: Vec<(String, bool)> = Settings::default()
                .share
                .into_iter()
                .map(|name| {
                    let on = settings.share.contains(&name);
                    (name, on)
                })
                .collect();
            for name in &settings.share {
                if !items.iter().any(|(n, _)| n == name) {
                    items.push((name.clone(), true));
                }
            }
            self.modal = Modal::Checklist { key, items, at: 0 };
        } else {
            let flipped = if current == "true" { "false" } else { "true" };
            let _ = jobs.send(Job::SetSetting(key.into(), flipped.into()));
        }
    }
}

// ---- the top strip --------------------------------------------------------

fn draw_fleet(frame: &mut Frame, area: Rect, snapshot: &Snapshot, app: &mut App) {
    let state = &snapshot.state;
    let fleet = fleet_of(snapshot);
    let count = match area.width {
        w if w >= 140 => 4,
        w if w >= 100 => 3,
        _ => 2,
    };
    let cards = Layout::horizontal(vec![Constraint::Fill(1); count]).split(area);
    let card = |frame: &mut Frame, slot: usize, title: &str, lines: Vec<Line<'static>>| {
        if let Some(rect) = cards.get(slot) {
            frame.render_widget(
                Paragraph::new(lines).block(panel(title).padding(Padding::horizontal(1))),
                *rect,
            );
        }
    };

    let who = match snapshot.live.and_then(|id| state.accounts.get(&id)) {
        Some(account) => vec![
            Line::from(vec![
                Span::styled("● ", Style::new().fg(good())),
                Span::styled(account.display(), bold()),
            ]),
            Line::from(Span::styled(
                format!(
                    "{}{}",
                    account.plan.clone().unwrap_or_else(|| "subscription".into()),
                    if app.auto { " · switching automatically" } else { "" }
                ),
                dim(),
            )),
        ],
        None if snapshot.live_unmanaged => vec![
            Line::from(Span::styled("not one of cx's accounts", Style::new().fg(warn()))),
            Line::from(Span::styled("press c to take it over", dim())),
        ],
        None => vec![
            Line::from(Span::styled("Claude Code is signed out", Style::new().fg(warn()))),
            Line::from(Span::styled("press Enter on an account", dim())),
        ],
    };
    card(frame, 0, "Claude Code is signed in as", who);

    let mut first = vec![Span::styled(
        format!("{} with room", fleet.ready),
        Style::new().fg(good()),
    )];
    if fleet.over > 0 {
        first.push(Span::styled("  ·  ", Style::new().fg(faint())));
        first.push(Span::styled(
            format!("{} past the switch line", fleet.over),
            Style::new().fg(bad()),
        ));
    }
    let mut second: Vec<Span> = Vec::new();
    for (n, label, color) in [
        (fleet.needs_login, "need sign-in", bad()),
        (fleet.disabled, "disabled", muted()),
        (fleet.sessions, "sessions open", accent()),
    ] {
        if n > 0 {
            if !second.is_empty() {
                second.push(Span::styled(" · ", Style::new().fg(faint())));
            }
            second.push(Span::styled(format!("{n} {label}"), Style::new().fg(color)));
        }
    }
    if second.is_empty() {
        second.push(Span::styled("nothing needs attention", dim()));
    }
    card(frame, 1, "Subscriptions", vec![Line::from(first), Line::from(second)]);

    if count >= 3 {
        let width = cards[2].width.saturating_sub(4) as usize;
        let lines = match fleet.week_used {
            Some(pct) => {
                let mut spans = bar(pct / 100.0, width.saturating_sub(11).clamp(6, 40), accent());
                spans.push(Span::styled(format!(" {pct:>3.0}% used"), bold()));
                vec![
                    Line::from(spans),
                    Line::from(Span::styled(
                        match fleet.week_reset {
                            Some((s, id)) => format!("next reset: #{id} in {}", human_duration(s)),
                            None => "no reset pending".into(),
                        },
                        dim(),
                    )),
                ]
            }
            None => vec![Line::from(Span::styled("no reading yet", dim()))],
        };
        card(frame, 2, "This week, every account together", lines);
    }
    if count >= 4 {
        // The model in use decides which single-model limit counts.
        let working = snapshot.working_model.as_deref().map(activity::model_name);
        let counted = state.settings.model.clone();
        let first = match &working {
            Some(name) => Line::from(vec![Span::raw("Last used: "), Span::styled(name.clone(), bold())]),
            None => Line::from(Span::styled("No recent Claude Code use found", dim())),
        };
        let second = match (snapshot.model_setting.as_deref(), counted.as_deref()) {
            (Some("auto"), Some("all")) => "every limit counts when choosing".to_string(),
            (Some("auto"), Some(model)) => format!("so {model} limits count when choosing"),
            (Some("auto"), None) => "so single-model limits are ignored".to_string(),
            (_, Some(model)) => format!("set to count: {model}"),
            (_, None) => "single-model limits are ignored".to_string(),
        };
        card(
            frame,
            3,
            "Model in use · click or m to change",
            vec![first, Line::from(Span::styled(second, Style::new().fg(violet())))],
        );
    }
    if count >= 4 {
        app.hits.push((cards[3], Hit::Setting("model")));
    }
}

// ---- the accounts table ---------------------------------------------------

/// How the account table uses the width it has.
struct TableLayout {
    bar: usize,
    full_names: bool,
    resets: bool,
    /// Whether any account reports a 5-hour window at all.
    five_hour: bool,
    models: Vec<String>,
    trend: u16,
    name_width: u16,
}

const STATUS_WIDTH: u16 = 22;

impl TableLayout {
    fn window_width(&self) -> u16 {
        self.bar as u16 + 5 + if self.resets { 9 } else { 0 }
    }

    fn needed(&self) -> u16 {
        let windows = 1 + self.five_hour as u16 + self.models.len() as u16;
        // Mark, number, name, plan, status, the windows, the trend, and a
        // divider before each group of limits.
        let dividers = 1 + (!self.models.is_empty()) as u16;
        let columns = 5 + windows + (self.trend > 0) as u16 + dividers;
        1 + 2
            + self.name_width
            + 8
            + windows * self.window_width()
            + self.trend
            + dividers
            + STATUS_WIDTH
            + 2 * columns
            + 2
            + 2
    }

    /// The richest layout that fits: the trend goes first, then the bars
    /// shrink, names drop to aliases, reset times go.
    fn fit(width: u16, snapshot: &Snapshot) -> TableLayout {
        let state = &snapshot.state;
        let longest = |full: bool| {
            state
                .accounts
                .values()
                .filter(|a| a.kind != Kind::ApiKey)
                .map(|a| if full { ui::account_name(a) } else { short_name(a) }.chars().count())
                .max()
                .unwrap_or(10)
                .clamp(8, 36) as u16
        };
        let five_hour = state
            .usage
            .values()
            .any(|r| r.usage.as_ref().is_some_and(|u| u.five_hour.is_some()));
        let all_models = state.model_names();
        let options = [
            (20, true, true, 2, 14),
            (16, true, true, 2, 12),
            (14, true, true, 2, 0),
            (12, true, true, 2, 0),
            (10, false, true, 2, 0),
            (8, false, true, 1, 0),
            (8, false, false, 1, 0),
            (6, false, false, 1, 0),
        ];
        let mut layout = None;
        for (bar, full_names, resets, models, trend) in options {
            let candidate = TableLayout {
                bar,
                full_names,
                resets,
                five_hour,
                models: all_models.iter().take(models).cloned().collect(),
                trend,
                name_width: longest(full_names),
            };
            let fits = candidate.needed() <= width;
            layout = Some(candidate);
            if fits {
                break;
            }
        }
        layout.unwrap()
    }
}

fn short_name(account: &Account) -> String {
    account.alias.clone().unwrap_or_else(|| account.label.clone())
}

fn window_cell(
    window: Option<&Window>,
    line: f64,
    now: i64,
    width: usize,
    resets: bool,
    one_model: bool,
) -> Line<'static> {
    let Some(window) = window else {
        return Line::from(Span::styled("none", Style::new().fg(faint())));
    };
    let Some(pct) = window.pct_at(now) else {
        return Line::from(Span::styled("unknown", dim()));
    };
    let color = limit_color(pct, line, one_model);
    let mut spans = bar(pct / 100.0, width, color);
    spans.push(Span::styled(format!(" {pct:>3.0}%"), Style::new().fg(color)));
    if let Some(reset) = window.resets_in_s(now).filter(|s| resets && *s > 0 && pct > 0.0) {
        spans.push(Span::styled(format!("  {}", human_duration(reset)), dim()));
    }
    Line::from(spans)
}

fn level_color(level: Level) -> Color {
    match level {
        Level::Calm => muted(),
        Level::Busy => warn(),
        Level::Over => bad(),
    }
}

/// A few words on where the account stands, and their colour. Limits on all
/// models and limits on one model are named apart.
fn row_status(snapshot: &Snapshot, account: &Account) -> (String, Color) {
    let now = snapshot.now_ms;
    let record = snapshot.state.usage.get(&account.id);
    let sessions = snapshot.sessions.get(&account.id).copied().unwrap_or(0);
    let (text, level) = ui::status_text(account, record, sessions, now);
    if account.needs_login {
        return (text, bad());
    }
    if account.disabled || account.kind != Kind::Oauth {
        return (text, muted());
    }
    let Some(usage) = record.and_then(|r| r.usage.as_ref()) else {
        return (text, level_color(level));
    };
    let limits = limits_of(usage, &snapshot.state.settings);
    let mut parts = Vec::new();
    let mut loudest = Standing::Ready;
    for one_model in [false, true] {
        if let Some((standing, name, reset)) = worst(&limits, one_model, now)
            && standing != Standing::Ready
        {
            let back = match (standing, reset) {
                (Standing::Full, Some(s)) => format!(" for {}", human_duration(s)),
                _ => String::new(),
            };
            parts.push(match standing {
                Standing::Low => format!("{name} nearly used"),
                Standing::Over => format!("{name} past line"),
                _ => format!("{name} used up{back}"),
            });
            loudest = loudest.max(standing);
        }
    }
    if !text.is_empty() {
        parts.push(text);
    }
    if parts.is_empty() {
        return if snapshot.live == Some(account.id) {
            ("in use".into(), good())
        } else {
            ("has room".into(), good())
        };
    }
    let color = match loudest {
        Standing::Full | Standing::Over => bad(),
        Standing::Low => warn(),
        Standing::Ready => level_color(level),
    };
    (parts.join(" · "), color)
}

fn draw_table(frame: &mut Frame, area: Rect, snapshot: &Snapshot, app: &mut App) {
    let state = &snapshot.state;
    let now = snapshot.now_ms;
    let layout = TableLayout::fit(area.width, snapshot);
    let (bar_width, resets) = (layout.bar, layout.resets);
    let name = |a: &Account| {
        if layout.full_names {
            ui::account_name(a)
        } else {
            short_name(a)
        }
    };
    let divider = || Cell::from(Span::styled("│", Style::new().fg(faint())));
    let ids = app.ids();
    let rows: Vec<Row> = ids
        .iter()
        .filter_map(|id| state.accounts.get(id))
        .map(|account| {
            let active = snapshot.live == Some(account.id);
            let usage = state.usage.get(&account.id).and_then(|r| r.usage.as_ref());
            let (status, status_color) = row_status(snapshot, account);
            let name_style = if active {
                bold()
            } else if account.disabled || account.needs_login {
                dim()
            } else {
                Style::new()
            };
            let plan = account.plan.clone().unwrap_or_else(|| match account.kind {
                Kind::SetupToken => "token".into(),
                _ => String::new(),
            });
            let mut cells = vec![
                Cell::from(if active {
                    Span::styled("●", Style::new().fg(good()))
                } else if account.needs_login {
                    Span::styled("!", Style::new().fg(bad()))
                } else {
                    Span::raw(" ")
                }),
                Cell::from(Span::styled(format!("{:>2}", account.id), dim())),
                Cell::from(Span::styled(name(account), name_style)),
                Cell::from(Span::styled(plan, dim())),
                divider(),
            ];
            let waiting = usage.is_none() && account.kind == Kind::Oauth && !account.needs_login;
            if layout.five_hour {
                cells.push(Cell::from(match usage {
                    Some(u) => window_cell(
                        u.five_hour.as_ref(),
                        state.settings.threshold_5h,
                        now,
                        bar_width,
                        resets,
                        false,
                    ),
                    None => Line::default(),
                }));
            }
            cells.push(Cell::from(match usage {
                Some(u) => window_cell(
                    u.seven_day.as_ref(),
                    state.settings.threshold_7d,
                    now,
                    bar_width,
                    resets,
                    false,
                ),
                None if waiting => Line::from(Span::styled("no reading yet", dim())),
                None => Line::default(),
            }));
            if !layout.models.is_empty() {
                cells.push(divider());
            }
            for model in &layout.models {
                cells.push(Cell::from(match usage {
                    Some(u) => window_cell(
                        u.model(model),
                        state.settings.threshold_7d,
                        now,
                        bar_width,
                        resets,
                        true,
                    ),
                    None => Line::default(),
                }));
            }
            if layout.trend > 0 {
                let series = app.history.series(account.id, &Key::Week, now - 169 * HOUR_MS);
                let values = history::buckets(&series, now - 168 * HOUR_MS, now, layout.trend as usize);
                cells.push(Cell::from(if account.kind == Kind::Oauth {
                    trend(&values, state.settings.threshold_7d)
                } else {
                    Line::default()
                }));
            }
            cells.push(Cell::from(Span::styled(status, Style::new().fg(status_color))));
            Row::new(cells)
        })
        .collect();

    // Two header lines: the kind of limit above, the window below.
    let window_width = layout.window_width();
    let head = |top: &str, bottom: &str, color: Color| {
        Cell::from(Text::from(vec![
            Line::from(Span::styled(
                top.to_string(),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(bottom.to_string(), Style::new().fg(muted()))),
        ]))
    };
    let bar_head = || {
        Cell::from(Text::from(vec![
            Line::from(Span::styled("│", Style::new().fg(faint()))),
            Line::from(Span::styled("│", Style::new().fg(faint()))),
        ]))
    };
    let mut widths = vec![
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(layout.name_width),
        Constraint::Length(8),
        Constraint::Length(1),
    ];
    let mut titles = vec![
        head("", "", muted()),
        head("", " #", muted()),
        head("", "ACCOUNT", muted()),
        head("", "PLAN", muted()),
        bar_head(),
    ];
    if layout.five_hour {
        widths.push(Constraint::Length(window_width));
        titles.push(head("ALL MODELS", "5-hour window", good()));
    }
    widths.push(Constraint::Length(window_width));
    titles.push(head(if layout.five_hour { "" } else { "ALL MODELS" }, "week", good()));
    if !layout.models.is_empty() {
        widths.push(Constraint::Length(1));
        titles.push(bar_head());
    }
    for model in &layout.models {
        widths.push(Constraint::Length(window_width));
        titles.push(head(&format!("{} ONLY", model.to_uppercase()), "week", violet()));
    }
    if layout.trend > 0 {
        widths.push(Constraint::Length(layout.trend));
        titles.push(head("", "week, 7 days", muted()));
    }
    widths.push(Constraint::Min(STATUS_WIDTH));
    titles.push(head("", "WHERE IT STANDS", muted()));

    let focused = app.focus == Focus::Accounts;
    let title = match app.sort {
        Sort::Number => "Subscriptions".to_string(),
        Sort::Room => "Subscriptions · most room first".to_string(),
    };
    let table = Table::new(rows, widths)
        .header(Row::new(titles).height(2))
        .column_spacing(2)
        .row_highlight_style(Style::new().bg(selected_bg()))
        .highlight_symbol(Span::styled(
            "▌ ",
            Style::new().fg(if focused { accent() } else { faint() }),
        ))
        .block(panel(title).border_style(Style::new().fg(if focused { muted() } else { faint() })));
    let position = app.selected().and_then(|id| ids.iter().position(|i| *i == id));
    app.table.select(position);
    frame.render_stateful_widget(table, area, &mut app.table);
    // Rows start under the border and the two header lines.
    let first_row = area.y + 3;
    for (i, id) in ids.iter().enumerate().skip(app.table.offset()) {
        let y = first_row + (i - app.table.offset()) as u16;
        if y + 1 >= area.y + area.height {
            break;
        }
        app.hits.push((
            Rect::new(area.x + 1, y, area.width.saturating_sub(2), 1),
            Hit::Account(*id),
        ));
    }
}

// ---- the selected account -------------------------------------------------

fn labelled(label: &str, rest: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![Span::styled(format!("  {label:<12}"), dim())];
    spans.extend(rest);
    Line::from(spans)
}

fn limit_line(account: u32, limit: &Limit, history: &History, now: i64, width: usize) -> Line<'static> {
    let label = match &limit.key {
        Key::FiveHour => "5-hour",
        _ => "Week",
    };
    let Some(pct) = limit.window.pct_at(now) else {
        return labelled(label, vec![Span::styled("no reading", dim())]);
    };
    let color = limit_color(pct, limit.line, limit.one_model());
    let mut spans = bar(pct / 100.0, width, color);
    spans.push(Span::styled(
        format!(" {pct:>3.0}% used"),
        Style::new().fg(color).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(format!(" · {:.0}% left", (100.0 - pct).max(0.0)), dim()));
    if let Some(s) = limit.window.resets_in_s(now).filter(|s| *s > 0) {
        let clock = chrono::Local::now() + chrono::Duration::seconds(s);
        spans.push(Span::styled(
            format!(" · resets in {} ({})", human_duration(s), clock.format("%a %H:%M")),
            dim(),
        ));
    }
    if let Some(forecast) = forecast_of(history, account, limit, now) {
        let (text, color) = forecast_text(&forecast);
        spans.push(Span::styled(" · ", dim()));
        spans.push(Span::styled(text, Style::new().fg(color)));
    }
    labelled(label, spans)
}

/// The selected account's limits, grouped by what they apply to.
fn limit_lines(snapshot: &Snapshot, account: &Account, history: &History, width: u16) -> Vec<Line<'static>> {
    let now = snapshot.now_ms;
    let settings = &snapshot.state.settings;
    let bar_width = (width.saturating_sub(100) as usize).clamp(16, 40);
    let mut lines = Vec::new();
    let usage = snapshot.state.usage.get(&account.id).and_then(|r| r.usage.as_ref());
    match (account.kind, usage) {
        (Kind::Oauth, Some(u)) => {
            let limits = limits_of(u, settings);
            lines.push(heading("All models", good()));
            let general: Vec<&Limit> = limits.iter().filter(|l| !l.one_model()).collect();
            if general.is_empty() {
                lines.push(labelled("", vec![Span::styled("no limit reported", dim())]));
            }
            for limit in general {
                lines.push(limit_line(account.id, limit, history, now, bar_width));
            }
            for limit in limits.iter().filter(|l| l.one_model()) {
                let Key::Model(name) = &limit.key else { continue };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} ONLY", name.to_uppercase()),
                        Style::new().fg(violet()).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if settings.counts_model(name) {
                            "   counts when choosing an account"
                        } else {
                            "   ignored when choosing: the model in use is not this one"
                        },
                        dim(),
                    ),
                ]));
                lines.push(limit_line(account.id, limit, history, now, bar_width));
            }
            if let Some(extra) = &u.extra {
                let mut spans = bar(extra.pct / 100.0, bar_width, accent());
                spans.push(Span::raw(format!(
                    " {:.2} of {:.2} {} this month",
                    extra.used, extra.limit, extra.currency
                )));
                lines.push(labelled("Extra usage", spans));
            }
        }
        (Kind::Oauth, None) => {
            let why = snapshot
                .state
                .usage
                .get(&account.id)
                .and_then(|r| r.error.clone())
                .map(|e| format!(": the server answered \"{e}\""))
                .unwrap_or_default();
            lines.push(Line::from(Span::styled(format!("No usage reading yet{why}."), dim())));
        }
        _ => lines.push(Line::from(
            "A setup token. It can run Claude Code but the server reports no usage for it.",
        )),
    }
    let token = if account.kind != Kind::Oauth {
        vec![Span::raw("token stored")]
    } else if account.needs_login {
        vec![Span::styled(
            "the refresh token was rejected. Press l to sign in again.",
            Style::new().fg(bad()),
        )]
    } else {
        let left = (account.token_expires_ms - snapshot.now_ms) / 1000;
        vec![
            Span::styled("signed in", Style::new().fg(good())),
            Span::raw(if left > 0 {
                format!(
                    " · access token good for {}, renewed automatically",
                    human_duration(left)
                )
            } else {
                " · access token expired; renewed on next use".to_string()
            }),
        ]
    };
    lines.push(labelled("Sign-in", token));
    let mut extras = Vec::new();
    let sessions = snapshot.sessions.get(&account.id).copied().unwrap_or(0);
    if sessions > 0 {
        extras.push(format!(
            "{sessions} session{} open",
            if sessions == 1 { "" } else { "s" }
        ));
    }
    if account.priority != 0 {
        extras.push(format!("priority {:+}", account.priority));
    }
    if account.disabled {
        extras.push("off: never chosen automatically".into());
    }
    let pins: Vec<&str> = snapshot
        .state
        .mappings
        .iter()
        .filter(|(_, id)| **id == account.id)
        .map(|(dir, _)| dir.as_str())
        .collect();
    if !pins.is_empty() {
        extras.push(format!("pinned: {}", pins.join(", ")));
    }
    if !extras.is_empty() {
        lines.push(labelled("Also", vec![Span::raw(extras.join(" · "))]));
    }
    lines
}

fn ellipsis(text: &str, width: usize) -> String {
    if text.chars().count() > width {
        text.chars().take(width.saturating_sub(1)).collect::<String>() + "…"
    } else {
        text.to_string()
    }
}

/// "mcp__server__tool" reads "tool (server)".
fn tool_name(raw: &str) -> String {
    match raw.strip_prefix("mcp__").and_then(|rest| rest.split_once("__")) {
        Some((server, tool)) => format!("{tool} ({server})"),
        None => raw.to_string(),
    }
}

/// A number short enough to sit on a bar: 290k, 1.2M.
fn short(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.0}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// A titled box. Returns the area inside it.
fn card(frame: &mut Frame, area: Rect, title: String) -> Rect {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(faint()))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(accent()).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// One row of a share table: a name, its share, and numbers.
struct ShareRow {
    name: String,
    share: f64,
    color: Color,
    numbers: Vec<String>,
}

/// A table with a header: name, a bar and a percentage for the share, then
/// a right-aligned column for each number. `heads` names the first column
/// and the number columns.
fn share_table(frame: &mut Frame, area: Rect, heads: &[&str], rows: Vec<ShareRow>) {
    if area.height < 2 || area.width < 24 {
        return;
    }
    let number_widths: Vec<u16> = heads[1..]
        .iter()
        .enumerate()
        .map(|(i, head)| {
            rows.iter()
                .map(|r| r.numbers[i].chars().count())
                .chain([head.chars().count()])
                .max()
                .unwrap_or(0) as u16
        })
        .collect();
    let bar_width = (area.width / 7).clamp(4, 14);
    let fixed = bar_width + 4 + number_widths.iter().sum::<u16>() + 2 * (2 + number_widths.len() as u16);
    let name_width = area.width.saturating_sub(fixed).max(6);
    let mut header = vec![Cell::from(heads[0].to_string()), Cell::from("share"), Cell::from("")];
    header.extend(
        heads[1..]
            .iter()
            .map(|head| Cell::from(Line::from(head.to_string()).right_aligned())),
    );
    let body = rows.into_iter().take(area.height as usize - 1).map(|row| {
        let mut cells = vec![
            Cell::from(ellipsis(&row.name, name_width as usize)),
            Cell::from(Line::from(bar(row.share, bar_width as usize, row.color))),
            Cell::from(Span::styled(format!("{:>3.0}%", row.share * 100.0), bold())),
        ];
        cells.extend(
            row.numbers
                .into_iter()
                .map(|n| Cell::from(Line::from(Span::styled(n, dim())).right_aligned())),
        );
        Row::new(cells)
    });
    let mut widths = vec![
        Constraint::Length(name_width),
        Constraint::Length(bar_width),
        Constraint::Length(4),
    ];
    widths.extend(number_widths.iter().map(|w| Constraint::Length(*w)));
    frame.render_widget(
        Table::new(body, widths)
            .header(Row::new(header).style(Style::new().fg(faint()).add_modifier(Modifier::BOLD)))
            .column_spacing(2),
        area,
    );
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum CardKind {
    Models,
    Who,
    Cache,
    Days,
    Projects,
    Tools,
    Hooks,
    Limits,
    Waste,
    Skills,
}

/// What the cards draw from.
struct Breakdown<'a> {
    summary: &'a Summary,
    daily: &'a [(String, Tokens)],
    history: &'a History,
    account: u32,
    models: Vec<String>,
    now: i64,
}

fn draw_card(frame: &mut Frame, area: Rect, kind: CardKind, b: &Breakdown) {
    let summary = b.summary;
    let total = &summary.total;
    let by_tokens = |name: String, tokens: &Tokens, color: Color| ShareRow {
        name,
        share: if total.weight() > 0.0 {
            tokens.weight() / total.weight()
        } else {
            0.0
        },
        color,
        numbers: vec![compact(tokens.requests), compact(tokens.output)],
    };
    match kind {
        CardKind::Models => {
            let inner = card(frame, area, "Which model".into());
            let rows = summary
                .models
                .iter()
                .map(|(name, tokens)| {
                    // A model with its own weekly limit is drawn in that limit's colour.
                    let limited = b.models.iter().any(|m| name.to_lowercase().contains(&m.to_lowercase()));
                    by_tokens(name.clone(), tokens, if limited { violet() } else { good() })
                })
                .collect();
            share_table(frame, inner, &["Model", "replies", "output"], rows);
        }
        CardKind::Who => {
            let share = if total.weight() > 0.0 {
                summary.subagents.weight() / total.weight() * 100.0
            } else {
                0.0
            };
            let inner = card(frame, area, format!("Who asked · subagents {share:.0}%"));
            let mut rows = vec![by_tokens("Main chat".into(), &summary.main, accent())];
            if summary.agents.is_empty() && summary.subagents.requests > 0 {
                rows.push(by_tokens("Subagents".into(), &summary.subagents, violet()));
            }
            for (name, tokens) in &summary.agents {
                rows.push(by_tokens(format!("Agent: {name}"), tokens, violet()));
            }
            share_table(frame, inner, &["Who", "replies", "output"], rows);
        }
        CardKind::Projects => {
            let inner = card(frame, area, format!("Which project · {}", summary.projects.len()));
            let rows = summary
                .projects
                .iter()
                .map(|(name, tokens)| by_tokens(name.clone(), tokens, accent()))
                .collect();
            share_table(frame, inner, &["Project", "replies", "output"], rows);
        }
        CardKind::Tools => {
            let calls: u64 = summary.tools.iter().map(|t| t.1).sum();
            let inner = card(frame, area, format!("Which tools · {} calls", compact(calls)));
            let rows = summary
                .tools
                .iter()
                .map(|(name, n)| ShareRow {
                    name: tool_name(name),
                    share: *n as f64 / calls.max(1) as f64,
                    color: accent(),
                    numbers: vec![compact(*n)],
                })
                .collect();
            share_table(frame, inner, &["Tool", "calls"], rows);
        }
        CardKind::Hooks => {
            let waited: u64 = summary.hooks.iter().map(|h| h.2).sum();
            let inner = card(
                frame,
                area,
                format!("Hooks · {} waiting", human_duration((waited / 1000) as i64)),
            );
            if summary.hooks.is_empty() {
                frame.render_widget(Paragraph::new(Span::styled("none ran", dim())), inner);
                return;
            }
            let rows = summary
                .hooks
                .iter()
                .map(|(event, runs, ms)| {
                    let each = ms / (*runs).max(1);
                    ShareRow {
                        name: event.clone(),
                        share: *ms as f64 / waited.max(1) as f64,
                        color: warn(),
                        numbers: vec![
                            compact(*runs),
                            human_duration((*ms / 1000) as i64),
                            if each >= 1000 {
                                format!("{:.1}s", each as f64 / 1000.0)
                            } else {
                                format!("{each}ms")
                            },
                        ],
                    }
                })
                .collect();
            share_table(frame, inner, &["Hook", "runs", "waited", "each"], rows);
        }
        CardKind::Cache => {
            let sent = total.input + total.cache_write + total.cache_read;
            let reused = total.cache_read as f64 / sent.max(1) as f64;
            let inner = card(frame, area, "Cache".into());
            let [gauge, _, table, notes] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(4),
                Constraint::Min(0),
            ])
            .areas(inner);
            frame.render_widget(
                LineGauge::default()
                    .ratio(reused.clamp(0.0, 1.0))
                    .label(Span::styled(format!("{:.0}% reused", reused * 100.0), bold()))
                    .filled_symbol("━")
                    .unfilled_symbol("─")
                    .filled_style(Style::new().fg(good()))
                    .unfilled_style(Style::new().fg(faint())),
                gauge,
            );
            let line = |label: &str, n: u64, note: &str, color: Color| {
                Row::new(vec![
                    Cell::from(Span::styled("━ ", Style::new().fg(color))),
                    Cell::from(label.to_string()),
                    Cell::from(Line::from(Span::styled(compact(n), bold())).right_aligned()),
                    Cell::from(Span::styled(note.to_string(), dim())),
                ])
            };
            frame.render_widget(
                Table::new(
                    [
                        line("Reused from cache", total.cache_read, "cheap", good()),
                        line("Written to cache", total.cache_write, "", warn()),
                        line("Sent uncached", total.input, "", warn()),
                        line("Claude's replies", total.output, "", accent()),
                    ],
                    [
                        Constraint::Length(2),
                        Constraint::Length(18),
                        Constraint::Length(7),
                        Constraint::Min(0),
                    ],
                )
                .column_spacing(1),
                table,
            );
            // The first reply of a conversation always writes the cache;
            // only the rebuilds after that are worth a warning.
            let (rebuilds, rebuilt) = summary.waste.avoidable_rebuilds();
            let mut lines = vec![if rebuilds > 0 {
                Line::from(Span::styled(
                    format!(
                        "Cache rebuilt {} mid-chat, {} tokens resent",
                        plural(rebuilds, "time", "times"),
                        compact(rebuilt)
                    ),
                    Style::new().fg(warn()),
                ))
            } else {
                Line::from(Span::styled("No cache rebuilds mid-chat", dim()))
            }];
            if summary.compactions > 0 {
                lines.push(Line::from(Span::styled(
                    format!(
                        "Conversation compacted {}",
                        plural(summary.compactions, "time", "times")
                    ),
                    dim(),
                )));
            }
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), notes);
        }
        CardKind::Waste => {
            let w = &summary.waste;
            let inner = card(frame, area, "Wasted work".into());
            let (rebuilds, rebuilt) = w.avoidable_rebuilds();
            let line = |label: &str, n: u64, note: String| {
                Row::new(vec![
                    Cell::from(label.to_string()),
                    Cell::from(
                        Line::from(Span::styled(
                            compact(n),
                            if n > 0 {
                                Style::new().fg(warn()).add_modifier(Modifier::BOLD)
                            } else {
                                dim()
                            },
                        ))
                        .right_aligned(),
                    ),
                    Cell::from(Span::styled(note, dim())),
                ])
            };
            let rows = [
                line(
                    "You pressed Escape",
                    w.stops + w.tool_stops,
                    format!("{} tokens written first", compact(w.lost_output)),
                ),
                line(
                    "You corrected it",
                    w.redirects + w.corrections,
                    format!("of {}", plural(w.prompts, "prompt", "prompts")),
                ),
                line(
                    "Chat summarised",
                    w.auto_summaries + w.asked_summaries,
                    format!("{} waiting", human_duration((w.summary_ms / 1000) as i64)),
                ),
                line("Cache rebuilt", rebuilds, format!("{} tokens resent", compact(rebuilt))),
            ];
            let [table, notes] = Layout::vertical([Constraint::Length(4), Constraint::Min(0)]).areas(inner);
            frame.render_widget(
                Table::new(
                    rows,
                    [Constraint::Length(18), Constraint::Length(5), Constraint::Min(0)],
                )
                .column_spacing(1),
                table,
            );
            let worst = w
                .rebuilds
                .iter()
                .filter(|(cause, _)| cause.as_str() != activity::cause::START)
                .max_by_key(|(_, (_, tokens))| *tokens);
            if let Some((cause, (count, tokens))) = worst {
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(Span::styled("Most rebuilds:", dim())),
                        Line::from(Span::styled(
                            format!(
                                "{cause} · {} · {} tokens",
                                plural(*count, "time", "times"),
                                compact(*tokens)
                            ),
                            Style::new().fg(warn()),
                        )),
                    ])
                    .wrap(Wrap { trim: true }),
                    notes,
                );
            }
        }
        CardKind::Skills => {
            let calls: u64 = summary.skills.iter().map(|s| s.1).sum();
            let inner = card(frame, area, format!("Skills started · {}", compact(calls)));
            if summary.skills.is_empty() {
                frame.render_widget(Paragraph::new(Span::styled("none started", dim())), inner);
                return;
            }
            let rows = summary
                .skills
                .iter()
                .map(|(name, n)| ShareRow {
                    name: name.clone(),
                    share: *n as f64 / calls.max(1) as f64,
                    color: violet(),
                    numbers: vec![compact(*n)],
                })
                .collect();
            share_table(frame, inner, &["Skill", "times"], rows);
        }
        CardKind::Days => {
            let inner = card(frame, area, "Output per day · tokens".into());
            let today = activity::today();
            let bars: Vec<Bar> = b
                .daily
                .iter()
                .map(|(day, tokens)| {
                    let name = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
                        .map(|d| d.format("%a").to_string())
                        .unwrap_or_default();
                    let color = if *day == today { accent() } else { muted() };
                    Bar::default()
                        .value(tokens.output)
                        .text_value(if tokens.output > 0 {
                            short(tokens.output)
                        } else {
                            String::new()
                        })
                        .label(Line::from(if *day == today { "today".to_string() } else { name }))
                        .style(Style::new().fg(color))
                        .value_style(Style::new().fg(Color::Black).bg(color))
                })
                .collect();
            let count = bars.len().max(1) as u16;
            let width = (inner.width.saturating_sub(count - 1) / count).clamp(3, 10);
            frame.render_widget(
                BarChart::default()
                    .data(BarGroup::default().bars(&bars))
                    .bar_width(width)
                    .bar_gap(1)
                    .label_style(dim()),
                inner,
            );
        }
        CardKind::Limits => {
            let inner = card(frame, area, "Limits over time · % used".into());
            let points = |key: &Key| -> Vec<(f64, f64)> {
                b.history
                    .series(b.account, key, b.now - 168 * HOUR_MS)
                    .into_iter()
                    .map(|(t, pct)| ((t - b.now) as f64 / HOUR_MS as f64, pct))
                    .collect()
            };
            let week = points(&Key::Week);
            let models: Vec<(String, Vec<(f64, f64)>)> = b
                .models
                .iter()
                .map(|name| (format!("{name} only"), points(&Key::Model(name.clone()))))
                .collect();
            let earliest = week
                .iter()
                .chain(models.iter().flat_map(|m| m.1.iter()))
                .map(|p| p.0)
                .fold(0.0, f64::min);
            if week.len() < 2 && models.iter().all(|m| m.1.len() < 2) {
                frame.render_widget(
                    Paragraph::new(Span::styled("Fills in as cx takes readings.", dim())).wrap(Wrap { trim: true }),
                    inner,
                );
                return;
            }
            let start = earliest.floor().clamp(-168.0, -1.0);
            let mut datasets = vec![
                Dataset::default()
                    .name("all models")
                    .marker(symbols::Marker::Braille)
                    .graph_type(GraphType::Line)
                    .style(Style::new().fg(good()))
                    .data(&week),
            ];
            for (name, data) in &models {
                datasets.push(
                    Dataset::default()
                        .name(name.clone())
                        .marker(symbols::Marker::Braille)
                        .graph_type(GraphType::Line)
                        .style(Style::new().fg(violet()))
                        .data(data),
                );
            }
            frame.render_widget(
                Chart::new(datasets)
                    .x_axis(
                        Axis::default()
                            .bounds([start, 0.0])
                            .labels([
                                format!("{} ago", human_duration((-start * 3600.0) as i64)),
                                "now".to_string(),
                            ])
                            .style(Style::new().fg(faint())),
                    )
                    .y_axis(
                        Axis::default()
                            .bounds([0.0, 100.0])
                            .labels(["0%", "50%", "100%"])
                            .style(Style::new().fg(faint())),
                    ),
                inner,
            );
        }
    }
}

fn draw_selected(frame: &mut Frame, area: Rect, snapshot: &Snapshot, app: &mut App) {
    let Some(account) = app.selected().and_then(|id| snapshot.state.accounts.get(&id)).cloned() else {
        return;
    };
    let mut title = format!("#{} {}", account.id, ui::account_name(&account));
    if let Some(plan) = &account.plan {
        title = format!("{title} · {plan}");
    }
    if !account.org_name.is_empty() && !account.org_name.contains(&account.label) {
        title = format!("{title} · {}", account.org_name);
    }
    let block = panel(title).padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let limits = limit_lines(snapshot, &account, &app.history, inner.width);
    let limits_height = (limits.len() as u16).min(inner.height);
    frame.render_widget(
        Paragraph::new(limits),
        Rect::new(inner.x, inner.y, inner.width, limits_height),
    );
    let rest = Rect::new(
        inner.x,
        inner.y + limits_height + 1,
        inner.width,
        inner.height.saturating_sub(limits_height + 1),
    );
    if rest.height < 4 || account.kind != Kind::Oauth {
        return;
    }
    // The heading, with the period as two tabs that take clicks.
    let today_only = app.today_only;
    let heading_text = "WHAT USED THIS ACCOUNT  ";
    frame.render_widget(
        Paragraph::new(Span::styled(
            heading_text,
            Style::new().fg(accent()).add_modifier(Modifier::BOLD),
        )),
        Rect::new(rest.x, rest.y, rest.width, 1),
    );
    let tabs_x = rest.x + heading_text.len() as u16;
    let tabs = [("Today", true), ("7 days", false)];
    let tabs_width: u16 = tabs.iter().map(|(t, _)| t.len() as u16 + 2).sum::<u16>() + 1;
    frame.render_widget(
        Tabs::new(tabs.iter().map(|(t, _)| *t))
            .select(if today_only { 0 } else { 1 })
            .style(Style::new().fg(muted()).bg(selected_bg()))
            .highlight_style(Style::new().fg(Color::Black).bg(accent()).add_modifier(Modifier::BOLD))
            .divider(" ")
            .padding(" ", " "),
        Rect::new(tabs_x, rest.y, tabs_width.min(rest.width), 1),
    );
    let mut x = tabs_x;
    for (label, today) in tabs {
        let width = label.len() as u16 + 2;
        app.hits.push((Rect::new(x, rest.y, width, 1), Hit::Period(today)));
        x += width + 1;
    }
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!(
                "  showing {} · from Claude Code's records on this Mac · p switches",
                if today_only { "today" } else { "the last 7 days" }
            ),
            dim(),
        )),
        Rect::new(x, rest.y, rest.width.saturating_sub(x - rest.x), 1),
    );
    let body = Rect::new(rest.x, rest.y + 1, rest.width, rest.height - 1);
    let message = |frame: &mut Frame, text: String| {
        frame.render_widget(
            Paragraph::new(Span::styled(text, dim())).wrap(Wrap { trim: true }),
            Rect::new(body.x, body.y + 1, body.width, body.height.saturating_sub(1)),
        );
    };
    if app.activity.is_none() {
        message(frame, "Reading Claude Code's records…".into());
        return;
    }
    let Some((summary, daily)) = app.summary_for(&account.uuid) else {
        return;
    };
    if summary.total.requests == 0 {
        message(
            frame,
            format!(
                "Nothing on this Mac ran on this account {}. Use from another computer or from claude.ai still counts against its limits but leaves no record here.",
                if today_only { "today" } else { "in the last 7 days" }
            ),
        );
        return;
    }
    // Cards in order of importance, as many rows of them as fit.
    let kinds = [
        CardKind::Models,
        CardKind::Who,
        CardKind::Cache,
        CardKind::Waste,
        CardKind::Projects,
        CardKind::Days,
        CardKind::Hooks,
        CardKind::Limits,
        CardKind::Tools,
        CardKind::Skills,
    ];
    let columns = match body.width {
        w if w >= 190 => 4,
        w if w >= 140 => 3,
        w if w >= 96 => 2,
        _ => 1,
    };
    let rows = kinds.len().div_ceil(columns).min((body.height / 7).max(1) as usize);
    let shown = &kinds[..(rows * columns).min(kinds.len())];
    let breakdown = Breakdown {
        summary: &summary,
        daily: &daily,
        history: &app.history,
        account: account.id,
        models: snapshot.state.model_names(),
        now: snapshot.now_ms,
    };
    let row_areas = Layout::vertical(vec![Constraint::Fill(1); rows]).split(body);
    for (group, row_area) in shown.chunks(columns).zip(row_areas.iter()) {
        let areas = Layout::horizontal(vec![Constraint::Fill(1); group.len()]).split(*row_area);
        for (kind, rect) in group.iter().zip(areas.iter()) {
            draw_card(frame, *rect, *kind, &breakdown);
        }
    }
}

// ---- the bottom row -------------------------------------------------------

/// Best account first, once for each kind of work.
fn draw_rotation(frame: &mut Frame, area: Rect, snapshot: &Snapshot) {
    let state = &snapshot.state;
    let block = panel("Which account next").padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let counted = state.settings.model.clone();
    let mut lanes: Vec<(String, Option<String>, Color)> = state
        .model_names()
        .into_iter()
        .map(|m| (format!("For {m} work"), Some(m), violet()))
        .collect();
    lanes.push((
        if lanes.is_empty() {
            "For any work".to_string()
        } else {
            "For other models".to_string()
        },
        None,
        good(),
    ));
    let areas = Layout::horizontal(vec![Constraint::Fill(1); lanes.len()]).split(inner);
    for ((title, model, color), rect) in lanes.into_iter().zip(areas.iter()) {
        let in_use = match (&counted, &model) {
            (Some(c), Some(m)) => c.eq_ignore_ascii_case(m) || c == "all",
            (None, None) => true,
            _ => false,
        };
        let mut lines = vec![Line::from(vec![
            Span::styled(
                title.to_uppercase(),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(if in_use { "  ← in use now" } else { "" }, dim()),
        ])];
        let name_width = (rect.width as usize).saturating_sub(24).clamp(8, 28);
        let mut place = 0;
        for verdict in lane(snapshot, model.as_deref()) {
            let Some(account) = state.accounts.get(&verdict.id).filter(|a| a.kind == Kind::Oauth) else {
                continue;
            };
            let name: String = format!("#{} {}", account.id, short_name(account))
                .chars()
                .take(name_width)
                .collect();
            let mut spans = Vec::new();
            match verdict.room.as_ref().filter(|_| usable(&verdict)) {
                Some(room) => {
                    place += 1;
                    spans.push(Span::styled(format!("{place:>2} "), Style::new().fg(color)));
                    spans.push(Span::styled(
                        format!("{name:<name_width$} "),
                        if snapshot.live == Some(account.id) {
                            bold()
                        } else {
                            Style::new()
                        },
                    ));
                    spans.extend(bar(room.min / 100.0, 8, color));
                    spans.push(Span::styled(format!(" {:>2.0}% room", room.min), bold()));
                }
                None => {
                    spans.push(Span::styled(" – ", Style::new().fg(faint())));
                    spans.push(Span::styled(format!("{name:<name_width$} "), dim()));
                    // Why it is out, in a word or two.
                    let why = match &verdict.room {
                        Some(room) if verdict.eligible && room.at_limit => format!("{} used up", room.window),
                        Some(room) if verdict.eligible => format!("{} past its line", room.window),
                        _ => verdict.note.clone(),
                    };
                    spans.push(Span::styled(why, Style::new().fg(faint())));
                }
            }
            lines.push(Line::from(spans));
        }
        frame.render_widget(Paragraph::new(lines), *rect);
    }
}

fn draw_keys_table(frame: &mut Frame, area: Rect, snapshot: &Snapshot, app: &mut App) {
    let state = &snapshot.state;
    let focused = app.focus == Focus::Keys;
    let fallback = state.settings.api_fallback;
    let ids = app.key_ids();
    let title = if ids.is_empty() {
        "API keys".to_string()
    } else if fallback {
        "API keys · used when every subscription is at its limit".to_string()
    } else {
        "API keys · fallback is off (Settings)".to_string()
    };
    let block = panel(title)
        .border_style(Style::new().fg(if focused { muted() } else { faint() }))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let selected = app.selected_key();
    let mut rows: Vec<(Line<'static>, bool, Hit)> = Vec::new();
    for id in &ids {
        let Some(account) = state.accounts.get(id) else {
            continue;
        };
        let chosen = selected == Some(*id);
        let state_word = if account.disabled {
            Span::styled("off", Style::new().fg(muted()))
        } else if fallback {
            Span::styled("on", Style::new().fg(good()))
        } else {
            Span::styled("stored", Style::new().fg(muted()))
        };
        let content = Line::from(vec![
            Span::styled(if chosen && focused { "▌ " } else { "  " }, Style::new().fg(accent())),
            Span::styled(format!("#{:<2} ", account.id), dim()),
            Span::raw(format!(
                "{:<18} ",
                short_name(account).chars().take(18).collect::<String>()
            )),
            Span::styled(
                if account.hint.is_empty() {
                    "sk-ant-…      ".to_string()
                } else {
                    format!("sk-ant-…{:<6}", account.hint)
                },
                dim(),
            ),
            Span::raw("  "),
            state_word,
        ]);
        rows.push((content, chosen, Hit::ApiKey(*id)));
    }
    let chosen = selected.is_none();
    rows.push((
        Line::from(vec![
            Span::styled(if chosen && focused { "▌ " } else { "  " }, Style::new().fg(accent())),
            Span::styled(
                "+ Add an API key",
                Style::new().fg(accent()).add_modifier(Modifier::BOLD),
            ),
            Span::styled("   click, or press k", dim()),
        ]),
        chosen,
        Hit::AddKey,
    ));
    let count = rows.len() as u16;
    for (i, (content, chosen, hit)) in rows.into_iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let rect = Rect::new(inner.x, y, inner.width, 1);
        let style = if chosen && focused {
            Style::new().bg(selected_bg())
        } else {
            Style::new()
        };
        frame.render_widget(Paragraph::new(content).style(style), rect);
        app.hits.push((rect, hit));
    }
    if ids.is_empty() && inner.height >= count + 3 {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "Billed per token. Once a key is stored, cx uses it only when every subscription is at its limit.",
                dim(),
            ))
            .wrap(Wrap { trim: true }),
            Rect::new(
                inner.x + 2,
                inner.y + count + 1,
                inner.width.saturating_sub(2),
                inner.height - count - 1,
            ),
        );
    }
}

fn draw_log(frame: &mut Frame, area: Rect, app: &App) {
    let lines: Vec<Line> = app
        .log
        .iter()
        .take(area.height.saturating_sub(2) as usize)
        .map(|(stamp, text)| {
            let color = if text.contains("failed") || text.starts_with("not ") || text.contains("not stored") {
                Style::new().fg(bad())
            } else if text.contains("switched") || text.contains("is now") {
                Style::new().fg(good())
            } else {
                Style::new()
            };
            Line::from(vec![
                Span::styled(format!("{stamp}  "), Style::new().fg(faint())),
                Span::styled(text.clone(), color),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(panel("What cx did").padding(Padding::horizontal(1))),
        area,
    );
}

fn draw_welcome(frame: &mut Frame, area: Rect) {
    let key = |k: &'static str| {
        Span::styled(
            format!("  {k:<4}"),
            Style::new().fg(accent()).add_modifier(Modifier::BOLD),
        )
    };
    let lines = vec![
        Line::default(),
        Line::from(Span::styled("No subscription accounts yet.", bold())),
        Line::default(),
        Line::from(vec![key("n"), Span::raw("sign in to an account in the browser")]),
        Line::from(vec![
            key("c"),
            Span::raw("take over the login Claude Code is using now"),
        ]),
        Line::from(vec![key("k"), Span::raw("add an API key")]),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(panel("Start here").padding(Padding::horizontal(2))),
        area,
    );
}

// ---- frame, header, footer ------------------------------------------------

fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.hits.clear();
    if area.width < 64 || area.height < 14 {
        frame.render_widget(
            Paragraph::new("cx needs a window of at least 64 by 14.").wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let Some(snapshot) = app.snapshot.clone() else {
        frame.render_widget(
            Paragraph::new(Span::styled("Reading accounts…", dim())).block(panel("cx")),
            area,
        );
        return;
    };
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(area);
    draw_header(frame, header, &snapshot, app);
    draw_footer(frame, footer, app);

    let rows = app.ids().len() as u16;
    if rows == 0 {
        let [welcome, keys] = Layout::vertical([Constraint::Min(8), Constraint::Length(7)]).areas(body);
        draw_welcome(frame, welcome);
        draw_keys_table(frame, keys, &snapshot, app);
    } else {
        // The table gets what it needs, the bottom row a fixed share, and
        // the selected account everything left.
        let fleet_height = if body.height >= 34 { 4 } else { 0 };
        let bottom_height = match body.height {
            h if h >= 50 => 11,
            h if h >= 34 => 9,
            h if h >= 26 => 6,
            _ => 0,
        };
        let table_height = (rows + 4)
            .min(body.height.saturating_sub(fleet_height + bottom_height + 8))
            .max(5);
        let [fleet, table, selected, bottom] = Layout::vertical([
            Constraint::Length(fleet_height),
            Constraint::Length(table_height),
            Constraint::Min(6),
            Constraint::Length(bottom_height),
        ])
        .areas(body);
        if fleet_height > 0 {
            draw_fleet(frame, fleet, &snapshot, app);
        }
        draw_table(frame, table, &snapshot, app);
        draw_selected(frame, selected, &snapshot, app);
        if bottom_height > 0 {
            if bottom.width >= 150 {
                let [rotation, keys, log] =
                    Layout::horizontal([Constraint::Fill(5), Constraint::Fill(3), Constraint::Fill(3)]).areas(bottom);
                draw_rotation(frame, rotation, &snapshot);
                draw_keys_table(frame, keys, &snapshot, app);
                draw_log(frame, log, app);
            } else {
                let [rotation, keys] = Layout::horizontal([Constraint::Fill(3), Constraint::Fill(2)]).areas(bottom);
                draw_rotation(frame, rotation, &snapshot);
                draw_keys_table(frame, keys, &snapshot, app);
            }
        }
    }
    if app.modal != Modal::None {
        // Only the pop-up takes clicks while it is open.
        app.hits.clear();
        draw_modal(frame, area, &snapshot, app);
    }
}

fn draw_header(frame: &mut Frame, area: Rect, snapshot: &Snapshot, app: &mut App) {
    let left = vec![
        Span::styled(
            " ◆ cx ",
            Style::new().fg(Color::Black).bg(accent()).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  Claude accounts", dim()),
    ];
    let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let chip = |key: &str| {
        Span::styled(
            format!(" {key} "),
            Style::new().fg(Color::Black).bg(muted()).add_modifier(Modifier::BOLD),
        )
    };
    let face = Style::new().bg(selected_bg());
    // Each piece, left to right, and what a click on it does.
    let mut pieces: Vec<(Span<'static>, Option<Hit>)> = Vec::new();
    if let Some(what) = &app.working {
        let tick = spinner[(app.started.elapsed().as_millis() / 100) as usize % spinner.len()];
        pieces.push((
            Span::styled(format!("{tick} {what}   "), Style::new().fg(accent())),
            None,
        ));
    }
    pieces.push((
        Span::styled(chrono::Local::now().format("%H:%M   ").to_string(), dim()),
        None,
    ));
    let gap = || (Span::raw(" "), None);
    pieces.push((chip("a"), Some(Hit::Button('a'))));
    pieces.push((
        if app.auto {
            Span::styled("Automatic switching: on ", face.fg(good()))
        } else {
            Span::styled("Automatic switching: off ", face)
        },
        Some(Hit::Button('a')),
    ));
    pieces.push(gap());
    pieces.push((
        Span::styled(format!(" Choosing by: {} ", snapshot.state.settings.strategy), face),
        Some(Hit::Setting("strategy")),
    ));
    for (key, label) in [('S', "Settings"), ('k', "API keys"), ('?', "Help")] {
        pieces.push(gap());
        pieces.push((chip(&key.to_string()), Some(Hit::Button(key))));
        pieces.push((Span::styled(format!("{label} "), face), Some(Hit::Button(key))));
    }
    let width: u16 = pieces.iter().map(|(span, _)| span.width() as u16).sum();
    let [left_area, right_area] = Layout::horizontal([Constraint::Min(0), Constraint::Length(width)]).areas(area);
    frame.render_widget(Paragraph::new(Line::from(left)), left_area);
    let mut x = right_area.x;
    let mut spans = Vec::new();
    for (span, hit) in pieces {
        let width = span.width() as u16;
        if let Some(hit) = hit {
            app.hits
                .push((Rect::new(x, area.y, width, 1).intersection(right_area), hit));
        }
        x += width;
        spans.push(span);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), right_area);
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let keys: &[(&str, &str)] = match (&app.modal, app.focus) {
        (Modal::None, Focus::Accounts) => &[
            ("↑↓", "choose"),
            ("Enter", "make default"),
            ("o", "open Claude Code on it"),
            ("b", "switch to best"),
            ("a", "automatic"),
            ("l", "sign in"),
            ("p", "today / 7 days"),
            ("Tab", "API keys"),
            ("S", "settings"),
            ("?", "all keys"),
            ("q", "quit"),
        ],
        (Modal::None, Focus::Keys) => &[
            ("↑↓", "choose"),
            ("Enter", "add, or open Claude Code on the key"),
            ("d", "on / off"),
            ("x", "remove"),
            ("Tab", "subscriptions"),
            ("?", "all keys"),
            ("q", "quit"),
        ],
        _ => &[("Esc", "close")],
    };
    let mut spans = vec![Span::raw(" ")];
    for (key, what) in keys {
        spans.push(Span::styled(
            key.to_string(),
            Style::new().fg(accent()).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {what}   "), dim()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ---- pop-ups --------------------------------------------------------------

/// A centred box with a title. Returns the area inside it.
fn popup(frame: &mut Frame, area: Rect, title: &str, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(4));
    let height = height.min(area.height.saturating_sub(2));
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(accent()))
        .padding(Padding::new(2, 2, 1, 1))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(accent()).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    inner
}

fn hint(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), dim()))
}

/// A list where one row is chosen. Rows take clicks.
fn draw_list(frame: &mut Frame, inner: Rect, rows: Vec<Line<'static>>, at: usize, app: &mut App) {
    for (i, row) in rows.into_iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let rect = Rect::new(inner.x, y, inner.width, 1);
        let mut spans = vec![Span::styled(
            if i == at { "▌ " } else { "  " },
            Style::new().fg(accent()),
        )];
        spans.extend(row.spans);
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(if i == at {
                Style::new().bg(selected_bg())
            } else {
                Style::new()
            }),
            rect,
        );
        app.hits.push((rect, Hit::ListRow(i)));
    }
}

fn draw_modal(frame: &mut Frame, area: Rect, snapshot: &Snapshot, app: &mut App) {
    match app.modal.clone() {
        Modal::None => {}
        Modal::Help => draw_help(frame, area),
        Modal::ConfirmRemove(id) => {
            let inner = popup(frame, area, "Remove", 60, 8);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!(
                        "Remove {} and delete what is stored for it?",
                        name_of(snapshot, id)
                    )),
                    Line::default(),
                    hint("y  remove          any other key  keep"),
                ])
                .wrap(Wrap { trim: true }),
                inner,
            );
        }
        Modal::Alias(id, typed) => {
            let inner = popup(frame, area, "Rename", 60, 10);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("A short name for {}.", name_of(snapshot, id))),
                    hint("Leave it empty to remove the name."),
                    Line::default(),
                    Line::from(vec![
                        Span::styled("› ", Style::new().fg(accent())),
                        Span::styled(format!("{typed}▏"), bold()),
                    ]),
                    Line::default(),
                    hint("Enter  save          Esc  cancel"),
                ]),
                inner,
            );
        }
        Modal::Settings => {
            let mut settings = snapshot.state.settings.clone();
            settings.model = snapshot.model_setting.clone();
            let count = store::SETTING_HELP.len();
            let inner = popup(frame, area, "Settings", 96, count as u16 + 8);
            let at = app.settings_row.min(count - 1);
            let rows: Vec<Line> = store::SETTING_HELP
                .iter()
                .map(|(key, _)| {
                    let value = settings.show(key).unwrap_or_default();
                    let shown: String = if value.chars().count() > 44 {
                        value.chars().take(43).collect::<String>() + "…"
                    } else {
                        value
                    };
                    let changed = !settings.is_default(key);
                    Line::from(vec![
                        Span::styled(format!("{:<28}", setting_title(key)), bold()),
                        Span::styled(
                            format!("{shown:<46}"),
                            Style::new().fg(if changed { warn() } else { accent() }),
                        ),
                        Span::styled(if changed { "changed" } else { "" }, dim()),
                    ])
                })
                .collect();
            let list = Rect::new(inner.x, inner.y, inner.width, count as u16);
            draw_list(frame, list, rows, at, app);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        store::SETTING_HELP[at].1.to_string(),
                        Style::new().fg(muted()),
                    )),
                    Line::default(),
                    hint("↑ ↓  choose      Enter  change      ⌫  back to the default      Esc  close"),
                ])
                .wrap(Wrap { trim: true }),
                Rect::new(
                    inner.x,
                    inner.y + count as u16 + 1,
                    inner.width,
                    inner.height.saturating_sub(count as u16 + 1),
                ),
            );
        }
        Modal::Number { key, value, typed } => {
            let inner = popup(frame, area, setting_title(key), 64, 13);
            let (step, low, high) = Settings::number_range(key).unwrap_or((1.0, 0.0, 100.0));
            let shown = if typed.is_empty() {
                format!("{value}")
            } else {
                format!("{typed}▏")
            };
            let centre = |text: String, style: Style| Line::from(Span::styled(text, style)).centered();
            let lines = vec![
                centre("▲".into(), Style::new().fg(accent())),
                centre(shown, bold().fg(accent())),
                centre("▼".into(), Style::new().fg(accent())),
                Line::default(),
                Line::from(Span::styled(setting_help(key).to_string(), dim())),
                hint(&format!("From {low} to {high}, in steps of {step}.")),
                Line::default(),
                hint("↑ ↓  change      type a number      Enter  save      Esc  cancel"),
            ];
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
            // The arrows take clicks: a band a few cells either side.
            let middle = inner.x + inner.width / 2;
            let band = |y: u16| Rect::new(middle.saturating_sub(4), y, 9, 1);
            app.hits.push((band(inner.y), Hit::Up));
            app.hits.push((band(inner.y + 2), Hit::Down));
        }
        Modal::Choice { key, options, at } => {
            let inner = popup(frame, area, setting_title(key), 68, options.len() as u16 + 6);
            let rows = options
                .iter()
                .map(|option| {
                    Line::from(vec![
                        Span::styled(format!("{option:<16}"), bold()),
                        Span::styled(choice_help(key, option), dim()),
                    ])
                })
                .collect();
            let list = Rect::new(inner.x, inner.y, inner.width, options.len() as u16);
            draw_list(frame, list, rows, at, app);
            frame.render_widget(
                Paragraph::new(hint("↑ ↓  choose      Enter  save      Esc  cancel")),
                Rect::new(inner.x, inner.y + options.len() as u16 + 1, inner.width, 1),
            );
        }
        Modal::Checklist { key, items, at } => {
            let inner = popup(frame, area, setting_title(key), 72, items.len() as u16 + 8);
            let rows = items
                .iter()
                .map(|(name, on)| {
                    Line::from(vec![
                        Span::styled(
                            if *on { "[x] " } else { "[ ] " },
                            Style::new().fg(if *on { good() } else { muted() }),
                        ),
                        Span::raw(name.clone()),
                    ])
                })
                .collect();
            let list = Rect::new(inner.x, inner.y, inner.width, items.len() as u16);
            draw_list(frame, list, rows, at, app);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(setting_help(key).to_string(), dim())),
                    hint("↑ ↓  choose      Space  on / off      Enter  save      Esc  cancel"),
                ])
                .wrap(Wrap { trim: true }),
                Rect::new(inner.x, inner.y + items.len() as u16 + 1, inner.width, 3),
            );
        }
        Modal::KeyInput {
            label,
            key,
            on_key,
            error,
        } => {
            let inner = popup(frame, area, "Add an API key", 76, 15);
            // The key is never shown: dots, and its last four characters.
            let count = key.chars().count();
            let masked = if count <= 4 {
                "•".repeat(count)
            } else {
                format!(
                    "{}{}",
                    "•".repeat((count - 4).min(28)),
                    key.chars().skip(count - 4).collect::<String>()
                )
            };
            let field = |name: &str, value: String, active: bool| {
                Line::from(vec![
                    Span::styled(if active { "▌ " } else { "  " }, Style::new().fg(accent())),
                    Span::styled(format!("{name:<6}"), dim()),
                    Span::styled(
                        format!("{value}{}", if active { "▏" } else { "" }),
                        if active { bold() } else { Style::new() },
                    ),
                ])
            };
            let lines = vec![
                hint("From console.anthropic.com. It starts with sk-ant- and is billed per token."),
                Line::default(),
                field("Name", label, !on_key),
                field("Key", masked, on_key),
                Line::default(),
                match error {
                    Some(text) => Line::from(Span::styled(text, Style::new().fg(bad()))),
                    None => hint("Paste the key. It goes to the macOS Keychain and is not shown again."),
                },
                hint("Storing a key turns on its use when every subscription is at its limit."),
                Line::default(),
                hint("Tab  other field      Enter  save      Esc  cancel"),
            ];
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
            app.hits
                .push((Rect::new(inner.x, inner.y + 2, inner.width, 1), Hit::ListRow(0)));
            app.hits
                .push((Rect::new(inner.x, inner.y + 3, inner.width, 1), Hit::ListRow(1)));
        }
    }
}

/// A setting's name in words.
fn setting_title(key: &str) -> &'static str {
    match key {
        "strategy" => "How to choose",
        "threshold_5h" => "Switch line, 5-hour",
        "threshold_7d" => "Switch line, weekly",
        "model" => "Single-model limits",
        "margin" => "Margin to switch",
        "cooldown_s" => "Pause between switches",
        "interval_s" => "Check every",
        "usage_ttl_s" => "Usage reading lasts",
        "api_fallback" => "Use API keys at the limit",
        "notify" => "Notifications",
        "share" => "Shared with sessions",
        _ => "Setting",
    }
}

fn setting_help(key: &str) -> &'static str {
    store::SETTING_HELP
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, help)| *help)
        .unwrap_or("")
}

fn choice_help(key: &str, option: &str) -> String {
    match (key, option) {
        ("strategy", "balanced") => "most room, spread over open sessions".into(),
        ("strategy", "headroom") => "most room, nothing else".into(),
        ("strategy", "drain-soonest") => "the week that resets soonest first".into(),
        ("model", "auto") => "count the limit of the model in use".into(),
        ("model", "all") => "count every single-model limit".into(),
        ("model", "none") => "ignore single-model limits".into(),
        ("model", name) => format!("always count the {name} limit"),
        _ => String::new(),
    }
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let inner = popup(frame, area, "Keys", 78, 32);
    let text = "\
↑ ↓          choose a row              Tab   subscriptions / API keys
Mouse        click a row or a button; the wheel moves the selection

Subscriptions
Enter        make it the default Claude Code login
o            open Claude Code on it here; the dashboard returns after
b            switch to the account with the most room
a            automatic switching on or off (while this is open)
p            what used it: today, or the last 7 days
s            sort by number or by room
t            renew its token now
l            sign in again              n   sign in to a new account
c            take over the login Claude Code is using now
e            give it a short name
d            keep it out of automatic choice, or return it
+  -         raise or lower its priority
x            remove it

API keys
k            add a key                  Enter   open Claude Code on a key
d            switch a key off or on     x       remove it

S            settings                   r   read usage again now
m            which single-model limit counts when choosing
q  Esc       quit

Green bars are limits on all models. Violet bars are limits on one model.
A bar turns amber within 25 points of its switch line and red past it.";
    frame.render_widget(Paragraph::new(text), inner);
}

// ---- input ----------------------------------------------------------------

/// Leave the dashboard, run something that needs the terminal, come back.
fn with_terminal_released(terminal: &mut ratatui::DefaultTerminal, action: impl FnOnce() -> String) -> Result<String> {
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    let message = action();
    println!("\n{message}\n\nPress Enter to return to the dashboard.");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    *terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    terminal.clear()?;
    Ok(message)
}

/// A key press while a pop-up is open. Returns whether one was.
fn modal_key(app: &mut App, code: KeyCode, jobs: &Sender<Job>) -> bool {
    let modal = std::mem::replace(&mut app.modal, Modal::None);
    match modal {
        Modal::None => return false,
        Modal::Help => {}
        Modal::ConfirmRemove(id) => {
            if code == KeyCode::Char('y') {
                let _ = jobs.send(Job::Remove(id));
            }
        }
        Modal::Alias(id, mut typed) => match code {
            KeyCode::Enter => {
                let name = typed.trim().to_string();
                let _ = jobs.send(Job::Alias(id, (!name.is_empty()).then_some(name)));
            }
            KeyCode::Esc => {}
            KeyCode::Backspace => {
                typed.pop();
                app.modal = Modal::Alias(id, typed);
            }
            KeyCode::Char(c) if typed.chars().count() < 24 && !c.is_whitespace() => {
                typed.push(c);
                app.modal = Modal::Alias(id, typed);
            }
            _ => app.modal = Modal::Alias(id, typed),
        },
        Modal::Settings => {
            let count = store::SETTING_HELP.len();
            app.modal = Modal::Settings;
            match code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('S') => app.modal = Modal::None,
                KeyCode::Up | KeyCode::Char('k') => app.settings_row = (app.settings_row + count - 1) % count,
                KeyCode::Down | KeyCode::Char('j') => app.settings_row = (app.settings_row + 1) % count,
                KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') => app.edit_setting(jobs),
                KeyCode::Backspace | KeyCode::Delete => {
                    let key = store::SETTING_HELP[app.settings_row.min(count - 1)].0;
                    if let Some(value) = Settings::default().show(key) {
                        let _ = jobs.send(Job::SetSetting(key.into(), value));
                    }
                }
                _ => {}
            }
        }
        Modal::Number { key, value, mut typed } => {
            let (step, low, high) = Settings::number_range(key).unwrap_or((1.0, 0.0, 100.0));
            let current = typed.parse::<f64>().unwrap_or(value);
            let moved = |by: f64| Modal::Number {
                key,
                value: (current + by).clamp(low, high),
                typed: String::new(),
            };
            match code {
                KeyCode::Esc => app.modal = app.back(),
                KeyCode::Enter => {
                    let _ = jobs.send(Job::SetSetting(key.into(), format!("{current}")));
                    app.modal = app.back();
                }
                KeyCode::Up | KeyCode::Char('+') | KeyCode::Right => app.modal = moved(step),
                KeyCode::Down | KeyCode::Char('-') | KeyCode::Left => app.modal = moved(-step),
                KeyCode::PageUp => app.modal = moved(step * 10.0),
                KeyCode::PageDown => app.modal = moved(-step * 10.0),
                KeyCode::Backspace => {
                    typed.pop();
                    app.modal = Modal::Number { key, value, typed };
                }
                KeyCode::Char(c) if c.is_ascii_digit() && typed.len() < 6 => {
                    typed.push(c);
                    app.modal = Modal::Number { key, value, typed };
                }
                _ => app.modal = Modal::Number { key, value, typed },
            }
        }
        Modal::Choice { key, options, at } => {
            let count = options.len();
            match code {
                KeyCode::Esc => app.modal = app.back(),
                KeyCode::Enter | KeyCode::Char(' ') => {
                    let _ = jobs.send(Job::SetSetting(key.into(), options[at].clone()));
                    app.modal = app.back();
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.modal = Modal::Choice {
                        key,
                        options,
                        at: (at + count - 1) % count,
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.modal = Modal::Choice {
                        key,
                        options,
                        at: (at + 1) % count,
                    }
                }
                _ => app.modal = Modal::Choice { key, options, at },
            }
        }
        Modal::Checklist { key, mut items, at } => {
            let count = items.len();
            match code {
                KeyCode::Esc => app.modal = app.back(),
                KeyCode::Enter => {
                    let chosen: Vec<&str> = items.iter().filter(|(_, on)| *on).map(|(n, _)| n.as_str()).collect();
                    let _ = jobs.send(Job::SetSetting(key.into(), chosen.join(",")));
                    app.modal = app.back();
                }
                KeyCode::Char(' ') | KeyCode::Right | KeyCode::Left => {
                    items[at].1 = !items[at].1;
                    app.modal = Modal::Checklist { key, items, at };
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.modal = Modal::Checklist {
                        key,
                        items,
                        at: (at + count - 1) % count,
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.modal = Modal::Checklist {
                        key,
                        items,
                        at: (at + 1) % count,
                    }
                }
                _ => app.modal = Modal::Checklist { key, items, at },
            }
        }
        Modal::KeyInput {
            mut label,
            mut key,
            mut on_key,
            mut error,
        } => {
            let mut close = false;
            match code {
                KeyCode::Esc => close = true,
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => on_key = !on_key,
                KeyCode::Enter if !on_key => on_key = true,
                KeyCode::Enter => {
                    if crate::cred::classify_token(key.trim()).is_none() {
                        error = Some("That is not an Anthropic key: it should start with sk-ant-.".into());
                    } else {
                        let _ = jobs.send(Job::AddKey {
                            label: label.clone(),
                            key: key.trim().to_string(),
                        });
                        close = true;
                    }
                }
                KeyCode::Backspace => {
                    if on_key {
                        key.pop();
                    } else {
                        label.pop();
                    }
                    error = None;
                }
                KeyCode::Char(c) if on_key && !c.is_whitespace() => {
                    key.push(c);
                    error = None;
                }
                KeyCode::Char(c) if !on_key && label.chars().count() < 24 => label.push(c),
                _ => {}
            }
            if !close {
                app.modal = Modal::KeyInput {
                    label,
                    key,
                    on_key,
                    error,
                };
            }
        }
    }
    true
}

/// A key press on the main screen.
fn main_key(app: &mut App, code: KeyCode, jobs: &Sender<Job>) -> Flow {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => return Flow::Quit,
        KeyCode::Tab | KeyCode::BackTab => {
            app.focus = match app.focus {
                Focus::Accounts => Focus::Keys,
                Focus::Keys => Focus::Accounts,
            }
        }
        KeyCode::Up => app.move_selection(-1),
        KeyCode::Down => app.move_selection(1),
        KeyCode::Char('?') | KeyCode::Char('h') => app.modal = Modal::Help,
        KeyCode::Char('S') => app.modal = Modal::Settings,
        KeyCode::Char('m') => {
            app.back_to_settings = false;
            app.open_editor("model", jobs);
        }
        KeyCode::Char('k') => app.open_key_input(),
        KeyCode::Char('r') => {
            let _ = jobs.send(Job::Refresh { force: true });
        }
        KeyCode::Char('a') => {
            app.auto = !app.auto;
            let _ = jobs.send(Job::SetAuto(app.auto));
        }
        KeyCode::Char('b') => {
            let _ = jobs.send(Job::SwitchBest);
        }
        KeyCode::Char('p') => app.today_only = !app.today_only,
        KeyCode::Char('n') => return Flow::SignIn { again: false },
        KeyCode::Char('c') => return Flow::TakeOver,
        _ => {
            let target = match app.focus {
                Focus::Accounts => app.selected(),
                Focus::Keys => app.selected_key(),
            };
            match (code, target, app.focus) {
                (KeyCode::Enter, None, Focus::Keys) => app.open_key_input(),
                (KeyCode::Enter, Some(id), Focus::Keys) => return Flow::OpenClaude(id),
                (KeyCode::Enter, Some(id), Focus::Accounts) => {
                    let _ = jobs.send(Job::Switch(id));
                }
                (KeyCode::Char('o'), Some(id), _) => return Flow::OpenClaude(id),
                (KeyCode::Char('d'), Some(id), _) => {
                    let _ = jobs.send(Job::ToggleDisabled(id));
                }
                (KeyCode::Char('x') | KeyCode::Delete, Some(id), _) => app.modal = Modal::ConfirmRemove(id),
                (KeyCode::Char('e'), Some(id), _) => {
                    let current = app
                        .snapshot
                        .as_ref()
                        .and_then(|s| s.state.accounts.get(&id))
                        .and_then(|a| a.alias.clone())
                        .unwrap_or_default();
                    app.modal = Modal::Alias(id, current);
                }
                (KeyCode::Char('s'), _, Focus::Accounts) => {
                    app.sort = match app.sort {
                        Sort::Number => Sort::Room,
                        Sort::Room => Sort::Number,
                    }
                }
                (KeyCode::Char('t'), Some(id), Focus::Accounts) => {
                    let _ = jobs.send(Job::RefreshToken(id));
                }
                (KeyCode::Char('+') | KeyCode::Char('='), Some(id), Focus::Accounts) => {
                    let _ = jobs.send(Job::Priority(id, 1));
                }
                (KeyCode::Char('-'), Some(id), Focus::Accounts) => {
                    let _ = jobs.send(Job::Priority(id, -1));
                }
                (KeyCode::Char('l'), Some(_), Focus::Accounts) => return Flow::SignIn { again: true },
                _ => {}
            }
        }
    }
    Flow::Continue
}

/// A click: on the main screen it selects, in a pop-up it chooses.
fn click(app: &mut App, hit: Hit, jobs: &Sender<Job>) -> Flow {
    match hit {
        Hit::Account(id) => {
            app.focus = Focus::Accounts;
            app.selected = Some(id);
        }
        Hit::ApiKey(id) => {
            app.focus = Focus::Keys;
            app.selected_key = Some(id);
        }
        Hit::AddKey => app.open_key_input(),
        Hit::Button(key) => return main_key(app, KeyCode::Char(key), jobs),
        Hit::Setting(key) => {
            app.back_to_settings = false;
            app.open_editor(key, jobs);
        }
        Hit::Period(today) => app.today_only = today,
        Hit::Up => {
            modal_key(app, KeyCode::Up, jobs);
        }
        Hit::Down => {
            modal_key(app, KeyCode::Down, jobs);
        }
        // A click chooses a row; a click on the chosen row acts on it.
        Hit::ListRow(row) => match app.modal.clone() {
            Modal::Settings if app.settings_row == row => app.edit_setting(jobs),
            Modal::Settings => app.settings_row = row,
            Modal::Choice { at, .. } if at == row => {
                modal_key(app, KeyCode::Enter, jobs);
            }
            Modal::Choice { key, options, .. } => app.modal = Modal::Choice { key, options, at: row },
            Modal::Checklist { key, mut items, .. } => {
                if let Some(item) = items.get_mut(row) {
                    item.1 = !item.1;
                }
                app.modal = Modal::Checklist { key, items, at: row };
            }
            Modal::KeyInput { label, key, error, .. } => {
                app.modal = Modal::KeyInput {
                    label,
                    key,
                    on_key: row == 1,
                    error,
                }
            }
            _ => {}
        },
    }
    Flow::Continue
}

/// Run the full-screen dashboard until the user quits.
///
/// The calling thread draws and reads keys and the mouse. A worker thread
/// with its own [`Engine`] does everything that touches the network, the
/// Keychain or a lock.
pub fn run(engine: Engine) -> Result<()> {
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let (update_tx, update_rx) = mpsc::channel::<Update>();
    let worker_engine = Engine::open()?;
    let worker_updates = update_tx.clone();
    let handle = std::thread::spawn(move || worker(worker_engine, job_rx, worker_updates));
    let store_root = engine.store.root.clone();
    std::thread::spawn(move || activity_reader(store_root, update_tx));

    let mut app = App::new();
    app.working = Some("starting".into());
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let result = (|| -> Result<()> {
        loop {
            while let Ok(update) = update_rx.try_recv() {
                match update {
                    Update::Snapshot(snapshot) => {
                        // Start on the account in use.
                        if app.snapshot.is_none() {
                            app.selected = snapshot.live;
                        }
                        app.snapshot = Some(*snapshot);
                    }
                    Update::History(history) => app.history = *history,
                    Update::Activity(activity) => {
                        app.activity = Some(activity);
                        app.summary = None;
                    }
                    Update::Log(text) => app.push_log(text),
                    Update::Working(what) => app.working = what,
                }
            }
            terminal.draw(|frame| draw(frame, &mut app))?;
            if !event::poll(Duration::from_millis(120))? {
                continue;
            }
            let flow = match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                        Flow::Quit
                    } else if modal_key(&mut app, key.code, &job_tx) {
                        Flow::Continue
                    } else {
                        main_key(&mut app, key.code, &job_tx)
                    }
                }
                // A pasted key arrives whole.
                Event::Paste(text) => {
                    if let Modal::KeyInput {
                        label,
                        key,
                        on_key,
                        error,
                    } = &mut app.modal
                    {
                        let text: String = text.chars().filter(|c| !c.is_control()).collect();
                        if *on_key {
                            key.push_str(text.trim());
                        } else {
                            label.push_str(text.trim());
                        }
                        *error = None;
                    }
                    Flow::Continue
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => match app.hit_at(mouse.column, mouse.row) {
                        Some(hit) => click(&mut app, hit, &job_tx),
                        None => Flow::Continue,
                    },
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let code = if mouse.kind == MouseEventKind::ScrollUp {
                            KeyCode::Up
                        } else {
                            KeyCode::Down
                        };
                        if !modal_key(&mut app, code, &job_tx) {
                            main_key(&mut app, code, &job_tx);
                        }
                        Flow::Continue
                    }
                    _ => Flow::Continue,
                },
                _ => Flow::Continue,
            };
            match flow {
                Flow::Continue => {}
                Flow::Quit => return Ok(()),
                Flow::TakeOver => {
                    match accounts::add_current(&engine) {
                        Ok((id, created)) => app.push_log(format!(
                            "{} account #{id} from Claude Code's current login",
                            if created { "added" } else { "updated" }
                        )),
                        Err(e) => app.push_log(format!("take-over failed: {e:#}")),
                    }
                    let _ = job_tx.send(Job::Refresh { force: false });
                }
                Flow::OpenClaude(id) => {
                    let message = with_terminal_released(&mut terminal, || {
                        let started = std::env::current_exe()
                            .and_then(|exe| std::process::Command::new(exe).args(["run", &id.to_string()]).status());
                        match started {
                            Ok(status) if status.success() => format!("Claude Code on #{id} ended"),
                            Ok(status) => format!("Claude Code on #{id} ended ({status})"),
                            Err(e) => format!("starting Claude Code failed: {e}"),
                        }
                    })?;
                    app.push_log(message);
                    let _ = job_tx.send(Job::Refresh { force: true });
                }
                Flow::SignIn { again } => {
                    let account = app
                        .snapshot
                        .as_ref()
                        .and_then(|s| app.selected().and_then(|id| s.state.accounts.get(&id).cloned()));
                    let email = account.filter(|a| a.kind == Kind::Oauth && again).map(|a| a.label);
                    let message = with_terminal_released(&mut terminal, || {
                        match &email {
                            Some(e) => {
                                println!("Signing in to {e}. Use the browser profile that belongs to this account.")
                            }
                            None => {
                                println!("Signing in to a new account. Use the browser profile that belongs to it.")
                            }
                        }
                        match accounts::login(&engine, email.as_deref(), false) {
                            Ok((id, created)) => {
                                format!("{} account #{id}", if created { "Added" } else { "Signed in again to" })
                            }
                            Err(e) => format!("Sign-in failed: {e:#}"),
                        }
                    })?;
                    app.push_log(message);
                    let _ = job_tx.send(Job::Refresh { force: false });
                }
            }
        }
    })();
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    let _ = job_tx.send(Job::Quit);
    // The worker may be mid-request; it is not worth making the user wait.
    drop(handle);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Sample;
    use crate::store::{ExtraUsage, ModelWindow, State, UsageRecord};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;

    fn later(hours: f64) -> Option<String> {
        let ms = now_ms() + (hours * 3_600_000.0) as i64;
        Some(chrono::DateTime::from_timestamp_millis(ms).unwrap().to_rfc3339())
    }

    /// Number, email, alias, plan, week %, Fable week %.
    type Seed = (u32, &'static str, Option<&'static str>, &'static str, f64, Option<f64>);

    fn fixture() -> Snapshot {
        let mut state = State::default();
        let rows: [Seed; 5] = [
            (1, "anna@studio.example", Some("studio"), "Max 20x", 31.0, Some(12.0)),
            (2, "anna@lab.example", None, "Max 20x", 64.0, Some(100.0)),
            (3, "ops@lab.example", Some("ops"), "Max 5x", 90.0, Some(40.0)),
            (4, "spare@lab.example", None, "Pro", 97.0, None),
            (5, "old@lab.example", None, "Max 5x", 5.0, None),
        ];
        for (id, label, alias, plan, week, fable) in rows {
            let mut account = Account::new(id, Kind::Oauth, label);
            account.alias = alias.map(String::from);
            account.plan = Some(plan.into());
            account.uuid = format!("u{id}");
            account.token_expires_ms = now_ms() + 5 * 3_600_000;
            state.accounts.insert(id, account);
            state.usage.insert(
                id,
                UsageRecord {
                    usage: Some(Usage {
                        five_hour: None,
                        seven_day: Some(Window {
                            pct: Some(week),
                            resets_at: later(80.0),
                        }),
                        models: fable
                            .map(|pct| {
                                vec![ModelWindow {
                                    name: "Fable".into(),
                                    window: Window {
                                        pct: Some(pct),
                                        resets_at: later(80.0),
                                    },
                                }]
                            })
                            .unwrap_or_default(),
                        extra: (id == 1).then(|| ExtraUsage {
                            used: 12.5,
                            limit: 50.0,
                            pct: 25.0,
                            currency: "USD".into(),
                        }),
                    }),
                    fetched_ms: now_ms(),
                    ..UsageRecord::default()
                },
            );
        }
        state.accounts.get_mut(&4).unwrap().disabled = true;
        state.accounts.get_mut(&5).unwrap().needs_login = true;
        let mut key = Account::new(6, Kind::ApiKey, "console");
        key.hint = "9xQz".into();
        state.accounts.insert(6, key);
        state.mappings.insert("/work/studio".into(), 1);
        // The model in use is Fable, so Fable limits count.
        state.settings.model = Some("Fable".into());
        let sessions: BTreeMap<u32, usize> = [(2, 2)].into_iter().collect();
        let verdicts = rank::evaluate(&state, &sessions, state.settings.strategy, now_ms());
        Snapshot {
            state,
            live: Some(1),
            live_unmanaged: false,
            sessions,
            verdicts,
            now_ms: now_ms(),
            working_model: Some("claude-fable-5-2".into()),
            model_setting: Some("auto".into()),
        }
    }

    /// Account 1's week climbing 6 points over the last five hours.
    fn climbing() -> History {
        let mut history = History::default();
        let now = now_ms();
        for (hours_ago, pct) in [(5, 25.0), (3, 28.0), (1, 31.0)] {
            history.by_account.entry(1).or_default().push(Sample {
                t: now - hours_ago * HOUR_MS,
                a: 1,
                w: Some(pct),
                ..Sample::default()
            });
        }
        history
    }

    /// Transcripts for account 1: Fable in the main chat, Haiku in a subagent.
    fn used() -> Arc<Activity> {
        let root = std::env::temp_dir().join(format!("cx-tui-{}-{}", std::process::id(), fastrand::u32(..)));
        let project = root.join("claude/projects/-work-studio");
        let agents = project.join("s1/subagents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::create_dir_all(root.join("store")).unwrap();
        let stamp = chrono::Utc::now().to_rfc3339();
        let reply = |request: &str, model: &str, out: u64, written: u64, read: u64| {
            serde_json::json!({"type": "assistant", "timestamp": stamp, "cwd": "/work/studio", "requestId": request,
                "message": {"model": model, "content": [{"type": "tool_use", "name": "Bash"}], "usage": {
                    "input_tokens": 100, "output_tokens": out,
                    "cache_creation_input_tokens": written, "cache_read_input_tokens": read}}})
            .to_string()
        };
        let lines = [
            serde_json::json!({"type": "session-owner", "accountUuid": "u1"}).to_string(),
            reply("r1", "claude-fable-5-2", 4000, 60_000, 0),
            reply("r2", "claude-fable-5-2", 2000, 500, 90_000),
            serde_json::json!({"type": "attachment", "timestamp": stamp, "attachment":
                {"type": "hook_success", "hookEvent": "PreToolUse", "durationMs": 95_000}})
            .to_string(),
        ];
        std::fs::write(project.join("s1.jsonl"), lines.join("\n") + "\n").unwrap();
        std::fs::write(
            agents.join("agent-a.jsonl"),
            reply("r3", "claude-haiku-5-5", 600, 0, 1000) + "\n",
        )
        .unwrap();
        std::fs::write(agents.join("agent-a.meta.json"), r#"{"agentType":"Explore"}"#).unwrap();
        let mut activity = Activity::default();
        activity.refresh(&root.join("claude"), &root.join("store"));
        Arc::new(activity)
    }

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn app() -> App {
        let mut app = App::new();
        app.snapshot = Some(fixture());
        app.history = climbing();
        app.activity = Some(used());
        app.selected = Some(1);
        app.auto = true;
        app.push_log("auto: staying (8 points of room on 7d)".into());
        app
    }

    fn jobs() -> (Sender<Job>, Receiver<Job>) {
        mpsc::channel()
    }

    #[test]
    fn one_screen_shows_the_accounts_the_breakdown_the_rotation_and_the_keys() {
        let mut app = app();
        let screen = render(&mut app, 200, 60);
        println!("{screen}");
        for text in [
            // The table keeps the two kinds of limit apart.
            "ALL MODELS",
            "FABLE ONLY",
            "studio (anna@studio.example)",
            "needs sign-in",
            "disabled",
            "2 sessions",
            "Fable used up",
            "week nearly used",
            // The selected account.
            "31% used · 69% left",
            "counts when choosing an account",
            "Extra usage",
            "pinned: /work/studio",
            // What used it, from the transcripts.
            "WHAT USED THIS ACCOUNT",
            "showing the last 7 days",
            "Which model",
            "Fable 5.2",
            "Haiku 5.5",
            "Main chat",
            "Explore",
            "work/studio",
            "Reused from cache",
            "Wasted work",
            "You pressed Escape",
            "60% reused",
            "PreToolUse",
            "Hooks · 1m waiting",
            "Output per day",
            "today",
            "Limits over time",
            "Which tools · 3 calls",
            "Agent: Explore",
            "Choosing by: balanced",
            "Automatic switching: on",
            "Bash",
            // The bottom row.
            "FOR FABLE WORK",
            "FOR OTHER MODELS",
            "in use now",
            "API keys · fallback is off",
            "sk-ant-…9xQz",
            "Add an API key",
            "What cx did",
            "Last used: Fable 5.2",
            "so Fable limits count when choosing",
        ] {
            assert!(screen.contains(text), "missing {text:?}");
        }
        // No account reports a 5-hour window, so there is no such column.
        assert!(!screen.contains("5-hour window"));
        // API keys are in their own table, not among the subscriptions.
        assert!(!screen.contains("billed per token"));
        // Nothing on screen sends the reader to the command line.
        assert!(!screen.contains("`cx"));
    }

    #[test]
    fn fable_and_other_work_rank_the_accounts_separately() {
        let snapshot = fixture();
        let order = |model: Option<&str>| -> Vec<u32> {
            lane(&snapshot, model)
                .iter()
                .filter(|v| usable(v))
                .map(|v| v.id)
                .collect()
        };
        // #2 has used all of its Fable week but has room for anything else.
        let (fable, other) = (order(Some("Fable")), order(None));
        assert_eq!(fable.first(), Some(&1));
        assert!(!fable.contains(&2) && other.contains(&2), "{fable:?} {other:?}");
    }

    #[test]
    fn the_breakdown_can_show_today_alone_and_says_when_there_is_nothing() {
        let mut app = app();
        app.today_only = true;
        assert!(render(&mut app, 200, 60).contains("showing today"));
        app.selected = Some(3);
        let screen = render(&mut app, 200, 60);
        assert!(
            screen.contains("Nothing on this Mac ran on this account today"),
            "{screen}"
        );
        app.activity = None;
        assert!(render(&mut app, 200, 60).contains("Reading Claude Code's records"));
    }

    #[test]
    fn it_fits_a_small_window() {
        let mut app = app();
        app.selected = Some(5);
        for (width, height) in [(100u16, 30u16), (120, 40), (160, 50)] {
            let screen = render(&mut app, width, height);
            for text in [
                "old@lab.example",
                "needs sign-in",
                "Max 20x",
                "refresh token was rejected",
            ] {
                assert!(screen.contains(text), "{width}x{height} lost {text:?}\n{screen}");
            }
        }
        assert!(render(&mut app, 50, 10).contains("at least 64 by 14"));
        let snapshot = fixture();
        for width in [100u16, 120, 150, 200] {
            let layout = TableLayout::fit(width, &snapshot);
            assert!(layout.needed() <= width, "width {width} needs {}", layout.needed());
        }
        assert!(TableLayout::fit(220, &snapshot).trend > 0);
    }

    #[test]
    fn settings_are_a_submenu_with_an_editor_for_each_kind_of_value() {
        let mut app = app();
        let (tx, rx) = jobs();
        main_key(&mut app, KeyCode::Char('S'), &tx);
        let screen = render(&mut app, 140, 44);
        println!("{screen}");
        for title in [
            "How to choose",
            "Switch line, weekly",
            "Single-model limits",
            "Shared with sessions",
        ] {
            assert!(screen.contains(title), "missing {title:?}");
        }
        // The stored setting is shown, not the settled one.
        assert!(screen.contains("auto"));

        // A number: up twice, save.
        app.settings_row = 1;
        modal_key(&mut app, KeyCode::Enter, &tx);
        assert!(matches!(&app.modal, Modal::Number { key: "threshold_5h", value, .. } if *value == 90.0));
        assert!(render(&mut app, 140, 44).contains("From 50 to 100, in steps of 1."));
        modal_key(&mut app, KeyCode::Up, &tx);
        modal_key(&mut app, KeyCode::Up, &tx);
        modal_key(&mut app, KeyCode::Enter, &tx);
        assert!(matches!(rx.try_recv(), Ok(Job::SetSetting(k, v)) if k == "threshold_5h" && v == "92"));
        assert_eq!(app.modal, Modal::Settings);
        // Nothing was sent for the steps in between.
        assert!(rx.try_recv().is_err());

        // A number can be typed, and stepping stops at the end of its range.
        modal_key(&mut app, KeyCode::Enter, &tx);
        for c in "75".chars() {
            modal_key(&mut app, KeyCode::Char(c), &tx);
        }
        modal_key(&mut app, KeyCode::Enter, &tx);
        assert!(matches!(rx.try_recv(), Ok(Job::SetSetting(_, v)) if v == "75"));
        modal_key(&mut app, KeyCode::Enter, &tx);
        for _ in 0..30 {
            modal_key(&mut app, KeyCode::Up, &tx);
        }
        assert!(matches!(&app.modal, Modal::Number { value, .. } if *value == 100.0));
        modal_key(&mut app, KeyCode::Esc, &tx);

        // A fixed few: a list.
        app.settings_row = 0;
        modal_key(&mut app, KeyCode::Enter, &tx);
        assert!(render(&mut app, 140, 44).contains("the week that resets soonest first"));
        modal_key(&mut app, KeyCode::Down, &tx);
        modal_key(&mut app, KeyCode::Enter, &tx);
        assert!(matches!(rx.try_recv(), Ok(Job::SetSetting(k, v)) if k == "strategy" && v == "headroom"));

        // On/off flips at once.
        app.settings_row = store::SETTING_HELP.iter().position(|(k, _)| *k == "notify").unwrap();
        modal_key(&mut app, KeyCode::Enter, &tx);
        assert!(matches!(rx.try_recv(), Ok(Job::SetSetting(k, v)) if k == "notify" && v == "false"));

        // The list of shared entries is a checklist.
        app.settings_row = store::SETTING_HELP.len() - 1;
        modal_key(&mut app, KeyCode::Enter, &tx);
        assert!(render(&mut app, 140, 44).contains("[x] settings.json"));
        modal_key(&mut app, KeyCode::Char(' '), &tx);
        modal_key(&mut app, KeyCode::Enter, &tx);
        assert!(
            matches!(rx.try_recv(), Ok(Job::SetSetting(k, v)) if k == "share" && v.starts_with("keybindings.json,"))
        );
        modal_key(&mut app, KeyCode::Esc, &tx);
        assert_eq!(app.modal, Modal::None);
    }

    #[test]
    fn a_key_is_entered_in_a_pop_up_and_never_shown() {
        let mut app = app();
        let (tx, rx) = jobs();
        main_key(&mut app, KeyCode::Char('k'), &tx);
        for c in "work".chars() {
            modal_key(&mut app, KeyCode::Char(c), &tx);
        }
        modal_key(&mut app, KeyCode::Enter, &tx);
        for c in "not-a-key".chars() {
            modal_key(&mut app, KeyCode::Char(c), &tx);
        }
        modal_key(&mut app, KeyCode::Enter, &tx);
        assert!(render(&mut app, 140, 44).contains("That is not an Anthropic key"));
        assert!(rx.try_recv().is_err());
        for _ in 0..9 {
            modal_key(&mut app, KeyCode::Backspace, &tx);
        }
        for c in "sk-ant-api03-secretsecretWXYZ".chars() {
            modal_key(&mut app, KeyCode::Char(c), &tx);
        }
        let screen = render(&mut app, 140, 44);
        assert!(screen.contains("•WXYZ") && !screen.contains("secretsecret"), "{screen}");
        modal_key(&mut app, KeyCode::Enter, &tx);
        assert!(matches!(rx.try_recv(), Ok(Job::AddKey { label, key })
                if label == "work" && key == "sk-ant-api03-secretsecretWXYZ"));
        assert_eq!(app.modal, Modal::None);
    }

    #[test]
    fn rows_and_buttons_take_clicks() {
        let mut app = app();
        let (tx, _rx) = jobs();
        render(&mut app, 200, 60);
        // Click where a thing was drawn, the way the mouse would.
        fn press(app: &mut App, wanted: Hit, tx: &Sender<Job>) {
            let (rect, _) = app
                .hits
                .iter()
                .find(|(_, hit)| *hit == wanted)
                .unwrap_or_else(|| panic!("{wanted:?} was not drawn"));
            let hit = app.hit_at(rect.x + 1, rect.y).unwrap();
            assert_eq!(hit, wanted);
            click(app, hit, tx);
        }
        // The header's mode text and the "Model in use" card open their own editors.
        press(&mut app, Hit::Setting("strategy"), &tx);
        assert!(matches!(&app.modal, Modal::Choice { key: "strategy", .. }));
        modal_key(&mut app, KeyCode::Esc, &tx);
        assert_eq!(app.modal, Modal::None);
        press(&mut app, Hit::Setting("model"), &tx);
        assert!(matches!(
            &app.modal,
            Modal::Choice {
                key: "model",
                at: 0,
                ..
            }
        ));
        modal_key(&mut app, KeyCode::Esc, &tx);
        main_key(&mut app, KeyCode::Char('m'), &tx);
        assert!(matches!(&app.modal, Modal::Choice { key: "model", .. }));
        modal_key(&mut app, KeyCode::Esc, &tx);
        // The period tabs.
        press(&mut app, Hit::Period(true), &tx);
        assert!(app.today_only);
        press(&mut app, Hit::Period(false), &tx);
        assert!(!app.today_only);
        press(&mut app, Hit::Account(3), &tx);
        assert_eq!(app.selected(), Some(3));
        // A key row moves the focus to the keys table.
        press(&mut app, Hit::ApiKey(6), &tx);
        assert_eq!((app.focus, app.selected_key()), (Focus::Keys, Some(6)));
        // "Add an API key" opens the entry pop-up.
        press(&mut app, Hit::AddKey, &tx);
        assert!(matches!(app.modal, Modal::KeyInput { .. }));
        app.modal = Modal::None;
        // The Settings button, then a row, then the same row again opens it.
        render(&mut app, 200, 60);
        press(&mut app, Hit::Button('S'), &tx);
        assert_eq!(app.modal, Modal::Settings);
        render(&mut app, 200, 60);
        // While a pop-up is open nothing behind it takes clicks.
        assert!(!app.hits.iter().any(|(_, hit)| matches!(hit, Hit::Account(_))));
        press(&mut app, Hit::ListRow(2), &tx);
        assert_eq!(app.settings_row, 2);
        press(&mut app, Hit::ListRow(2), &tx);
        assert!(matches!(
            &app.modal,
            Modal::Number {
                key: "threshold_7d",
                ..
            }
        ));
        // The arrows of the number editor.
        render(&mut app, 200, 60);
        press(&mut app, Hit::Down, &tx);
        assert!(matches!(&app.modal, Modal::Number { value, .. } if *value == 94.0));
    }

    #[test]
    fn selection_moves_within_the_focused_table() {
        let mut app = app();
        // Five subscriptions; the key is not among them.
        assert_eq!(app.ids(), vec![1, 2, 3, 4, 5]);
        app.move_selection(-1);
        assert_eq!(app.selected(), Some(5));
        app.sort = Sort::Room;
        assert_eq!(app.selected(), Some(5));
        assert_eq!(app.ids().first(), Some(&1));
        app.focus = Focus::Keys;
        assert_eq!(app.selected_key(), None);
        app.move_selection(1);
        assert_eq!(app.selected_key(), Some(6));
        app.move_selection(1);
        assert_eq!(app.selected_key(), None);
    }

    #[test]
    fn an_empty_store_points_at_the_first_steps() {
        let mut app = app();
        let mut snapshot = fixture();
        snapshot.state.accounts.clear();
        snapshot.live = None;
        app.snapshot = Some(snapshot);
        let screen = render(&mut app, 100, 30);
        assert!(
            screen.contains("sign in to an account") && screen.contains("Add an API key"),
            "{screen}"
        );
        assert!(!screen.contains("`cx"));
    }

    #[test]
    fn the_cards_shrink_to_what_fits() {
        let mut app = app();
        // Two columns and room for two rows: the four most important cards.
        let screen = render(&mut app, 120, 50);
        println!("{screen}");
        for text in ["Which model", "Who asked", "Cache", "Wasted work"] {
            assert!(screen.contains(text), "missing {text:?}");
        }
        assert_eq!(
            tool_name("mcp__pw-protoworks__browser_click"),
            "browser_click (pw-protoworks)"
        );
        assert_eq!(tool_name("Bash"), "Bash");
        assert_eq!(
            (short(950), short(290_500), short(1_234_567)),
            ("950".into(), "290k".into(), "1.2M".into())
        );
        assert_eq!(ellipsis("abcdef", 4), "abc…");
    }

    #[test]
    fn bars_and_numbers_read_plainly() {
        assert_eq!(bar_cells(0.0, 10), 0);
        assert_eq!(bar_cells(0.004, 10), 1);
        assert_eq!(bar_cells(0.5, 10), 5);
        assert_eq!(bar_cells(1.4, 10), 10);
        let text: String = bar(0.3, 10, good()).iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "━━━───────");
        assert_eq!(compact(999), "999");
        assert_eq!(compact(60_000), "60.0k");
        assert_eq!(compact(143_139_315), "143.1M");
        assert_eq!(compact(1_962_743_487), "1.96B");
        assert_eq!(standing(50.0, 95.0), Standing::Ready);
        assert_eq!(standing(90.0, 95.0), Standing::Low);
        assert_eq!(standing(96.0, 95.0), Standing::Over);
        assert_eq!(standing(100.0, 95.0), Standing::Full);
    }

    #[test]
    fn the_fleet_card_counts_accounts_and_weights_use_by_plan() {
        let fleet = fleet_of(&fixture());
        // #2 has used up Fable, which counts; #4 is disabled; #5 needs sign-in.
        assert_eq!(
            (fleet.ready, fleet.over, fleet.disabled, fleet.needs_login),
            (2, 1, 1, 1)
        );
        assert_eq!(fleet.sessions, 2);
        // (31*20 + 64*20 + 90*5) / 45
        assert!((fleet.week_used.unwrap() - 52.22).abs() < 0.01, "{:?}", fleet.week_used);
    }
}
