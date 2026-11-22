//! `cx menubar`: the accounts in the macOS menu bar.
//!
//! The item's title is the active account's fullest limit. Its menu lists
//! every account with its usage; choosing one makes it the default login.
//! AppKit owns the main thread, so everything that touches the network, the
//! Keychain or a lock runs on a worker thread, as in the dashboard.

use crate::engine::{Engine, Snapshot};
use crate::notify;
use crate::rank::Decision;
use crate::service::Unit;
use crate::store::Kind;
use anyhow::Result;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

/// The LaunchAgent that keeps `cx menubar` running.
pub const UNIT: Unit = Unit {
    label: "dev.cx.menubar",
    argument: "menubar",
    log: "menubar.log",
    what: "The menu-bar item",
};

const EVERY: Duration = Duration::from_secs(60);

enum Wake {
    Menu(MenuEvent),
    Fresh(Box<Snapshot>),
}

enum Job {
    Reload,
    Switch(u32),
    Best,
    Auto(bool),
}

/// The fullest limit of an account, as a percentage.
fn fullest(snapshot: &Snapshot, id: u32) -> Option<f64> {
    let usage = snapshot.state.usage.get(&id)?.usage.as_ref()?;
    let now = snapshot.now_ms;
    let settings = &snapshot.state.settings;
    usage
        .five_hour
        .iter()
        .chain(usage.seven_day.iter())
        .chain(
            usage
                .models
                .iter()
                .filter(|m| settings.counts_model(&m.name))
                .map(|m| &m.window),
        )
        .filter_map(|w| w.pct_at(now))
        .reduce(f64::max)
}

/// What the menu bar shows: the active account's fullest limit.
pub fn title_of(snapshot: &Snapshot) -> String {
    let Some(account) = snapshot.live.and_then(|id| snapshot.state.accounts.get(&id)) else {
        return "◆ cx".into();
    };
    if account.needs_login {
        return "◆ sign in".into();
    }
    match fullest(snapshot, account.id) {
        Some(pct) => format!("◆ {pct:.0}%"),
        None => "◆ cx".into(),
    }
}

/// One menu line per account: the menu id and the text.
pub fn account_lines(snapshot: &Snapshot) -> Vec<(String, String)> {
    let now = snapshot.now_ms;
    snapshot
        .state
        .accounts
        .values()
        .map(|account| {
            let mark = if snapshot.live == Some(account.id) {
                "●"
            } else {
                "   "
            };
            let name = match &account.alias {
                Some(alias) => alias.clone(),
                None => account.label.clone(),
            };
            let usage = snapshot.state.usage.get(&account.id).and_then(|r| r.usage.as_ref());
            let detail = if account.needs_login {
                "needs sign-in".to_string()
            } else if account.kind != Kind::Oauth {
                "billed per token".to_string()
            } else {
                match usage {
                    Some(u) => {
                        let mut parts = Vec::new();
                        if let Some(p) = u.five_hour.as_ref().and_then(|w| w.pct_at(now)) {
                            parts.push(format!("5h {p:.0}%"));
                        }
                        if let Some(p) = u.seven_day.as_ref().and_then(|w| w.pct_at(now)) {
                            parts.push(format!("week {p:.0}%"));
                        }
                        for model in &u.models {
                            if let Some(p) = model.window.pct_at(now) {
                                parts.push(format!("{} {p:.0}%", model.name));
                            }
                        }
                        parts.join(" · ")
                    }
                    None => "no reading yet".to_string(),
                }
            };
            let off = if account.disabled { " (disabled)" } else { "" };
            (
                format!("use:{}", account.id),
                format!("{mark} #{} {name}{off}    {detail}", account.id),
            )
        })
        .collect()
}

fn menu_of(snapshot: &Snapshot, auto: bool) -> Menu {
    let menu = Menu::new();
    for (id, text) in account_lines(snapshot) {
        let _ = menu.append(&MenuItem::with_id(id, text, true, None));
    }
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id(
        "best",
        "Switch to the account with most room",
        true,
        None,
    ));
    let _ = menu.append(&CheckMenuItem::with_id(
        "auto",
        "Switch automatically",
        true,
        auto,
        None,
    ));
    let _ = menu.append(&MenuItem::with_id("reload", "Reload usage", true, None));
    let _ = menu.append(&MenuItem::with_id("dashboard", "Open the dashboard", true, None));
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id("quit", "Quit", true, None));
    menu
}

fn worker(engine: Engine, jobs: Receiver<Job>, wake: EventLoopProxy<Wake>) {
    let mut auto = false;
    let mut watch = notify::Watch::default();
    let mut force = false;
    loop {
        if auto && let Ok((snapshot, decision, outcome)) = engine.auto_step(false) {
            let moved = match (&decision, &outcome) {
                (Decision::Switch { to, reason }, Some(_)) => Some((*to, reason.as_str())),
                _ => None,
            };
            watch.observe(&snapshot, moved);
        }
        if let Ok(snapshot) = engine.snapshot(force, true) {
            let _ = wake.send_event(Wake::Fresh(Box::new(snapshot)));
        }
        force = false;
        match jobs.recv_timeout(EVERY) {
            Ok(Job::Reload) => force = true,
            Ok(Job::Switch(id)) => {
                if let Err(e) = engine.switch_to(id) {
                    notify::send("cx", &format!("Switch failed: {e:#}"));
                }
            }
            Ok(Job::Best) => {
                let best = engine.snapshot(false, true).ok().and_then(|s| engine.pick(None, &s));
                match best {
                    Some(v) => {
                        if let Err(e) = engine.switch_to(v.id) {
                            notify::send("cx", &format!("Switch failed: {e:#}"));
                        }
                    }
                    None => notify::send("cx", "No account has room right now."),
                }
            }
            Ok(Job::Auto(on)) => auto = on,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Open the dashboard in a new Terminal window.
fn open_dashboard() {
    let Ok(exe) = std::env::current_exe() else { return };
    // Quoted once for the shell (a single quote inside is closed, escaped
    // and reopened), then escaped for the AppleScript string around it.
    let shell = format!("'{}'", exe.to_string_lossy().replace('\'', "'\\''"));
    let command = shell.replace('\\', "\\\\").replace('"', "\\\"");
    let _ = std::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            &format!("tell application \"Terminal\" to do script \"{command}\""),
            "-e",
            "tell application \"Terminal\" to activate",
        ])
        .spawn();
}

/// Show the menu-bar item and run its event loop. This takes over the
/// calling thread, which must be the main thread, and does not return: the
/// process exits when the user chooses Quit. A worker thread does every
/// read and switch.
pub fn run() -> Result<()> {
    let engine = Engine::open()?;
    let mut event_loop = EventLoopBuilder::<Wake>::with_user_event().build();
    // A menu-bar item only: no Dock icon, no app menu.
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = proxy.send_event(Wake::Menu(event));
    }));
    let (jobs, inbox) = mpsc::channel::<Job>();
    let proxy = event_loop.create_proxy();
    std::thread::spawn(move || worker(engine, inbox, proxy));

    let mut tray: Option<TrayIcon> = None;
    let mut auto = false;
    let mut last: Option<Snapshot> = None;
    event_loop.run(move |event, _, control| {
        *control = ControlFlow::Wait;
        match event {
            // The item can only be made once AppKit's loop is running.
            Event::NewEvents(StartCause::Init) => {
                tray = TrayIconBuilder::new()
                    .with_title("◆ cx")
                    .with_tooltip("cx: Claude accounts")
                    .build()
                    .ok();
            }
            Event::UserEvent(Wake::Fresh(snapshot)) => {
                if let Some(tray) = &tray {
                    tray.set_title(Some(title_of(&snapshot)));
                    tray.set_menu(Some(Box::new(menu_of(&snapshot, auto))));
                }
                last = Some(*snapshot);
            }
            Event::UserEvent(Wake::Menu(event)) => {
                let job = match event.id.0.as_str() {
                    "quit" => {
                        *control = ControlFlow::Exit;
                        None
                    }
                    "dashboard" => {
                        open_dashboard();
                        None
                    }
                    "reload" => Some(Job::Reload),
                    "best" => Some(Job::Best),
                    "auto" => {
                        auto = !auto;
                        if let (Some(tray), Some(snapshot)) = (&tray, &last) {
                            tray.set_menu(Some(Box::new(menu_of(snapshot, auto))));
                        }
                        Some(Job::Auto(auto))
                    }
                    other => other.strip_prefix("use:").and_then(|n| n.parse().ok()).map(Job::Switch),
                };
                if let Some(job) = job {
                    let _ = jobs.send(job);
                }
            }
            _ => {}
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Account, State, Usage, UsageRecord, Window};
    use std::collections::BTreeMap;

    fn snapshot() -> Snapshot {
        let mut state = State::default();
        let now = crate::util::now_ms();
        for (id, label, five, week) in [(1, "anna@lab.example", 82.0, 31.0), (2, "ops@lab.example", 5.0, 64.0)] {
            state.accounts.insert(id, Account::new(id, Kind::Oauth, label));
            state.usage.insert(
                id,
                UsageRecord {
                    usage: Some(Usage {
                        five_hour: Some(Window {
                            pct: Some(five),
                            resets_at: None,
                        }),
                        seven_day: Some(Window {
                            pct: Some(week),
                            resets_at: None,
                        }),
                        ..Usage::default()
                    }),
                    fetched_ms: now,
                    ..UsageRecord::default()
                },
            );
        }
        state.accounts.get_mut(&2).unwrap().alias = Some("ops".into());
        state.accounts.insert(3, Account::new(3, Kind::ApiKey, "console"));
        Snapshot {
            state,
            live: Some(1),
            live_unmanaged: false,
            sessions: BTreeMap::new(),
            verdicts: Vec::new(),
            now_ms: now,
            working_model: None,
            model_setting: Some("all".into()),
        }
    }

    #[test]
    fn the_title_is_the_active_accounts_fullest_limit() {
        let mut s = snapshot();
        assert_eq!(title_of(&s), "◆ 82%");
        s.live = Some(2);
        assert_eq!(title_of(&s), "◆ 64%");
        s.state.accounts.get_mut(&2).unwrap().needs_login = true;
        assert_eq!(title_of(&s), "◆ sign in");
        s.live = None;
        assert_eq!(title_of(&s), "◆ cx");
    }

    #[test]
    fn the_menu_has_a_line_per_account() {
        let lines = account_lines(&snapshot());
        assert_eq!(
            lines[0],
            (
                "use:1".to_string(),
                "● #1 anna@lab.example    5h 82% · week 31%".to_string()
            )
        );
        assert_eq!(lines[1].1, "    #2 ops    5h 5% · week 64%");
        assert_eq!(
            lines[2],
            ("use:3".to_string(), "    #3 console    billed per token".to_string())
        );
    }
}
