//! Which account to use: pure functions over usage readings, so every rule
//! here is tested without a network or a Keychain.
//!
//! An account is judged by its **room**: how many percentage points remain
//! under its switch lines, taking the tightest window. The 5-hour window and
//! the weekly windows have separate lines because they are different kinds
//! of budget: the 5-hour window returns in hours, the weekly one does not.

use crate::store::{Account, Kind, Settings, State, Strategy, Usage};
use std::collections::BTreeMap;

/// How far an account is from its switch lines at one moment. See the module
/// header for what room means.
#[derive(Clone, Debug, PartialEq)]
pub struct Room {
    /// Points left under the tightest line. Negative when over it.
    pub min: f64,
    /// Which window is the tightest: "5h", "7d", or "Fable week" for a
    /// model's own limit. "?" when
    /// the reading has no window with a percentage.
    pub window: String,
    /// Some window is fully used: requests are being refused.
    pub at_limit: bool,
    /// Seconds until the weekly window resets.
    pub week_resets_in_s: Option<i64>,
    /// Percent of the weekly window still unused.
    pub week_left: Option<f64>,
    /// Seconds until the tightest window resets.
    pub resets_in_s: Option<i64>,
}

/// Measure a usage reading against the switch lines in `settings`.
///
/// The 5-hour window is held to `threshold_5h`; the weekly window and the
/// weekly windows of the models `settings` counts are held to `threshold_7d`.
/// `now_ms` is in milliseconds since the Unix epoch. A reading with no
/// usable window gives a room of 0, which ranks as over the line.
pub fn room(usage: &Usage, settings: &Settings, now_ms: i64) -> Room {
    let mut windows: Vec<(String, f64, f64, Option<i64>)> = Vec::new();
    if let Some(w) = &usage.five_hour
        && let Some(pct) = w.pct_at(now_ms)
    {
        windows.push(("5h".into(), pct, settings.threshold_5h, w.resets_in_s(now_ms)));
    }
    if let Some(w) = &usage.seven_day
        && let Some(pct) = w.pct_at(now_ms)
    {
        windows.push(("7d".into(), pct, settings.threshold_7d, w.resets_in_s(now_ms)));
    }
    for m in usage.models.iter().filter(|m| settings.counts_model(&m.name)) {
        if let Some(pct) = m.window.pct_at(now_ms) {
            windows.push((
                format!("{} week", m.name),
                pct,
                settings.threshold_7d,
                m.window.resets_in_s(now_ms),
            ));
        }
    }
    let week = usage.seven_day.as_ref();
    let tightest = windows.iter().min_by(|a, b| (a.2 - a.1).total_cmp(&(b.2 - b.1)));
    Room {
        min: tightest.map(|w| w.2 - w.1).unwrap_or(0.0),
        window: tightest.map(|w| w.0.clone()).unwrap_or_else(|| "?".into()),
        at_limit: windows.iter().any(|w| w.1 >= 100.0),
        week_resets_in_s: week.and_then(|w| w.resets_in_s(now_ms)).filter(|s| *s > 0),
        week_left: week.and_then(|w| w.pct_at(now_ms)).map(|p| (100.0 - p).max(0.0)),
        resets_in_s: tightest.and_then(|w| w.3),
    }
}

/// The ranking's judgement of one account, as produced by [`evaluate`].
#[derive(Clone, Debug)]
pub struct Verdict {
    /// Number of the account judged.
    pub id: u32,
    /// May be chosen automatically.
    pub eligible: bool,
    /// Why not, or what the score rests on. Shown by `cx why`.
    pub note: String,
    /// Room under the switch lines. `None` when the account has no usage
    /// reading yet or is not eligible.
    pub room: Option<Room>,
    /// Higher is better. Only comparable between verdicts from one call to
    /// [`evaluate`], because it depends on the strategy. `f64::MIN` when not
    /// eligible.
    pub score: f64,
}

/// Room assumed for an account with no usage reading yet: enough to be
/// tried, not enough to beat an account known to be free.
const UNKNOWN_ROOM: f64 = 20.0;
/// Points one running session costs an account under the balanced strategy.
const SESSION_COST: f64 = 12.0;
/// Weekly quota that resets within this long counts as about to be lost.
const DRAIN_HORIZON_S: f64 = 48.0 * 3600.0;

fn score(room: &Room, sessions: usize, settings: &Settings, strategy: Strategy) -> f64 {
    if room.at_limit {
        return -2000.0 + room.min;
    }
    if room.min <= 0.0 {
        return -1000.0 + room.min;
    }
    match strategy {
        Strategy::Headroom => room.min,
        Strategy::Balanced => {
            let mut s = room.min - SESSION_COST * sessions as f64;
            if let (Some(reset), Some(left)) = (room.week_resets_in_s, room.week_left) {
                let reset = reset as f64;
                if reset < DRAIN_HORIZON_S && room.min >= settings.margin {
                    // Up to 20 points for quota that is both large and about to lapse.
                    s += (1.0 - reset / DRAIN_HORIZON_S) * left.min(40.0) * 0.5;
                }
            }
            s
        }
        Strategy::DrainSoonest => match room.week_resets_in_s {
            Some(reset) if room.min >= settings.margin => 500.0 - (reset as f64 / 3600.0).min(400.0),
            _ => room.min,
        },
    }
}

/// Judge every account. Best first; accounts that cannot be chosen
/// automatically come last.
pub fn evaluate(state: &State, sessions: &BTreeMap<u32, usize>, strategy: Strategy, now_ms: i64) -> Vec<Verdict> {
    let settings = &state.settings;
    let mut verdicts: Vec<Verdict> = state
        .accounts
        .values()
        .map(|account| judge(account, state, sessions, strategy, now_ms, settings))
        .collect();
    verdicts.sort_by(|a, b| {
        b.eligible
            .cmp(&a.eligible)
            .then(b.score.total_cmp(&a.score))
            .then(a.id.cmp(&b.id))
    });
    verdicts
}

fn judge(
    account: &Account,
    state: &State,
    sessions: &BTreeMap<u32, usize>,
    strategy: Strategy,
    now_ms: i64,
    settings: &Settings,
) -> Verdict {
    let running = sessions.get(&account.id).copied().unwrap_or(0);
    let blocked = |note: &str| Verdict {
        id: account.id,
        eligible: false,
        note: note.into(),
        room: None,
        score: f64::MIN,
    };
    if account.kind != Kind::Oauth {
        return blocked("not a subscription login");
    }
    if account.disabled {
        return blocked("disabled");
    }
    if account.needs_login {
        return blocked("needs sign-in");
    }
    let usage = state.usage.get(&account.id).and_then(|r| r.usage.as_ref());
    let (room, base, mut note) = match usage {
        Some(u) => {
            let r = room(u, settings, now_ms);
            let s = score(&r, running, settings, strategy);
            let note = if r.at_limit {
                format!("{} is at its limit", r.window)
            } else if r.min <= 0.0 {
                format!("{} is over its switch line", r.window)
            } else {
                format!("{:.0} points of room on {}", r.min, r.window)
            };
            (Some(r), s, note)
        }
        None => (
            None,
            UNKNOWN_ROOM - SESSION_COST * running as f64,
            "no usage reading yet".to_string(),
        ),
    };
    if running > 0 && strategy == Strategy::Balanced {
        note.push_str(&format!(
            ", {running} session{} running",
            if running == 1 { "" } else { "s" }
        ));
    }
    if account.priority != 0 {
        note.push_str(&format!(", priority {}", account.priority));
    }
    Verdict {
        id: account.id,
        eligible: true,
        note,
        room,
        score: base + 10_000.0 * account.priority as f64,
    }
}

/// What [`decide`] concluded for one pass of automatic switching.
#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    /// Keep the default login where it is. The text says why.
    Stay(String),
    /// Move the default login.
    Switch {
        /// Number of the account to switch to.
        to: u32,
        /// Why, in a few words, for the log and the notification.
        reason: String,
    },
}

/// Should the default login move, and where to?
///
/// The rule is to stay put while the active account has room. Every request
/// reuses the conversation cache built on the account that served the last
/// one, and a switch throws that cache away, so moving early costs tokens.
pub fn decide(
    active: Option<u32>,
    verdicts: &[Verdict],
    settings: &Settings,
    now_ms: i64,
    last_switch_ms: i64,
) -> Decision {
    let Some(active_id) = active else {
        return Decision::Stay("the default login is not an account cx manages".into());
    };
    let Some(current) = verdicts.iter().find(|v| v.id == active_id) else {
        return Decision::Stay("the active account is unknown".into());
    };
    let cooling = now_ms - last_switch_ms < settings.cooldown_s as i64 * 1000;
    let others = || verdicts.iter().filter(|v| v.eligible && v.id != active_id);

    let Some(current_room) = &current.room else {
        return if current.eligible {
            Decision::Stay("no usage reading for the active account yet".into())
        } else {
            match others().find(|v| v.room.as_ref().is_some_and(|r| r.min > 0.0)) {
                Some(best) => Decision::Switch {
                    to: best.id,
                    reason: format!("active account: {}", current.note),
                },
                None => Decision::Stay(format!("active account: {}; no other account has room", current.note)),
            }
        };
    };

    if current.eligible && current_room.min > 0.0 {
        if settings.strategy == Strategy::DrainSoonest && !cooling {
            // The one strategy that moves early: towards weekly quota that
            // will otherwise lapse.
            if let Some(best) = others().next() {
                let sooner = match (
                    best.room.as_ref().and_then(|r| r.week_resets_in_s),
                    current_room.week_resets_in_s,
                ) {
                    (Some(b), Some(c)) => b + 3600 < c,
                    _ => false,
                };
                if sooner && best.room.as_ref().is_some_and(|r| r.min >= settings.margin) {
                    return Decision::Switch {
                        to: best.id,
                        reason: "its weekly quota resets sooner and would lapse unused".into(),
                    };
                }
            }
        }
        return Decision::Stay(format!(
            "{:.0} points of room on {}",
            current_room.min, current_room.window
        ));
    }

    // The active account is over a line, at a limit, disabled or signed out.
    let reason = if current.eligible {
        current.note.clone()
    } else {
        format!("active account: {}", current.note)
    };
    let forced = current_room.at_limit || !current.eligible;
    if cooling && !forced {
        return Decision::Stay(format!("{reason}; waiting out the cooldown"));
    }
    let with_room = others().find(|v| v.room.as_ref().is_none_or(|r| r.min > 0.0) && v.score > -900.0);
    if let Some(best) = with_room {
        let gain = best.room.as_ref().map(|r| r.min).unwrap_or(UNKNOWN_ROOM) - current_room.min;
        if forced || gain >= settings.margin {
            return Decision::Switch { to: best.id, reason };
        }
        return Decision::Stay(format!(
            "{reason}; the best alternative is not better by {} points",
            settings.margin
        ));
    }
    // Nothing is under its lines. Moving is only worth it when the active
    // account is refusing requests and another one is not.
    if current_room.at_limit
        && let Some(best) = others().find(|v| v.room.as_ref().is_some_and(|r| !r.at_limit))
    {
        return Decision::Switch {
            to: best.id,
            reason: format!("{reason}; every account is over its line"),
        };
    }
    Decision::Stay(format!("{reason}; no other account has room"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{UsageRecord, Window};

    const NOW: i64 = 1_800_000_000_000;

    fn at(hours: f64) -> Option<String> {
        let ms = NOW + (hours * 3600.0 * 1000.0) as i64;
        Some(chrono::DateTime::from_timestamp_millis(ms).unwrap().to_rfc3339())
    }

    fn account(id: u32) -> Account {
        let mut a = Account::new(id, Kind::Oauth, &format!("a{id}@example.test"));
        a.uuid = format!("u{id}");
        a
    }

    /// (id, 5h %, 7d %, hours until the weekly reset)
    fn state(rows: &[(u32, f64, f64, f64)]) -> State {
        let mut s = State::default();
        for (id, five, week, week_reset_h) in rows {
            s.accounts.insert(*id, account(*id));
            s.usage.insert(
                *id,
                UsageRecord {
                    usage: Some(Usage {
                        five_hour: Some(Window {
                            pct: Some(*five),
                            resets_at: at(2.0),
                        }),
                        seven_day: Some(Window {
                            pct: Some(*week),
                            resets_at: at(*week_reset_h),
                        }),
                        ..Usage::default()
                    }),
                    fetched_ms: NOW,
                    ..UsageRecord::default()
                },
            );
        }
        s
    }

    fn order(s: &State, strategy: Strategy, sessions: &[(u32, usize)]) -> Vec<u32> {
        let map: BTreeMap<u32, usize> = sessions.iter().copied().collect();
        evaluate(s, &map, strategy, NOW)
            .iter()
            .filter(|v| v.eligible)
            .map(|v| v.id)
            .collect()
    }

    fn decision(s: &State, active: u32, last_switch_ms: i64) -> Decision {
        let v = evaluate(s, &BTreeMap::new(), s.settings.strategy, NOW);
        decide(Some(active), &v, &s.settings, NOW, last_switch_ms)
    }

    #[test]
    fn room_is_the_tightest_window_against_its_own_line() {
        let s = state(&[(1, 80.0, 93.0, 100.0)]);
        let r = room(s.usage[&1].usage.as_ref().unwrap(), &s.settings, NOW);
        // 5h: 90 - 80 = 10. 7d: 95 - 93 = 2.
        assert_eq!(r.window, "7d");
        assert!((r.min - 2.0).abs() < 1e-9);
        assert!(!r.at_limit);
    }

    #[test]
    fn a_model_weekly_limit_counts_unless_switched_off() {
        let mut s = state(&[(1, 10.0, 10.0, 100.0), (2, 30.0, 30.0, 100.0)]);
        s.usage
            .get_mut(&1)
            .unwrap()
            .usage
            .as_mut()
            .unwrap()
            .models
            .push(crate::store::ModelWindow {
                name: "Fable".into(),
                window: Window {
                    pct: Some(97.0),
                    resets_at: at(100.0),
                },
            });
        // By default every model limit counts: #1 is over its line on Fable.
        let r = room(s.usage[&1].usage.as_ref().unwrap(), &s.settings, NOW);
        assert_eq!(r.window, "Fable week");
        assert!(r.min < 0.0);
        assert_eq!(order(&s, Strategy::Headroom, &[]), vec![2, 1]);
        assert!(matches!(decision(&s, 1, 0), Decision::Switch { to: 2, .. }));
        // Naming another model, or none, takes Fable out of the judgement.
        for model in [Some("Opus".to_string()), None] {
            s.settings.model = model;
            assert_eq!(order(&s, Strategy::Headroom, &[]), vec![1, 2]);
        }
        s.settings.model = Some("fable".into());
        assert_eq!(order(&s, Strategy::Headroom, &[]), vec![2, 1]);
    }

    #[test]
    fn headroom_orders_by_room() {
        let s = state(&[(1, 50.0, 10.0, 100.0), (2, 10.0, 20.0, 100.0), (3, 95.0, 10.0, 100.0)]);
        assert_eq!(order(&s, Strategy::Headroom, &[]), vec![2, 1, 3]);
    }

    #[test]
    fn balanced_spreads_sessions_across_accounts() {
        let s = state(&[(1, 10.0, 10.0, 100.0), (2, 20.0, 20.0, 100.0)]);
        assert_eq!(order(&s, Strategy::Balanced, &[]), vec![1, 2]);
        // One session already on #1 costs it 12 points: 68 against 70.
        assert_eq!(order(&s, Strategy::Balanced, &[(1, 1)]), vec![2, 1]);
    }

    #[test]
    fn balanced_prefers_weekly_quota_about_to_lapse() {
        // Same room on both, but #2's week resets in 6 hours with 60% unused.
        let s = state(&[(1, 30.0, 40.0, 120.0), (2, 30.0, 40.0, 6.0)]);
        assert_eq!(order(&s, Strategy::Balanced, &[]), vec![2, 1]);
    }

    #[test]
    fn drain_soonest_takes_the_earliest_weekly_reset_that_has_room() {
        let s = state(&[(1, 10.0, 10.0, 100.0), (2, 50.0, 60.0, 20.0), (3, 10.0, 94.0, 5.0)]);
        // #3 resets soonest but has 1 point of room, under the margin.
        assert_eq!(order(&s, Strategy::DrainSoonest, &[]), vec![2, 1, 3]);
    }

    #[test]
    fn disabled_signed_out_and_key_accounts_are_never_chosen() {
        let mut s = state(&[
            (1, 10.0, 10.0, 100.0),
            (2, 10.0, 10.0, 100.0),
            (3, 10.0, 10.0, 100.0),
            (4, 50.0, 50.0, 100.0),
        ]);
        s.accounts.get_mut(&1).unwrap().disabled = true;
        s.accounts.get_mut(&2).unwrap().needs_login = true;
        s.accounts.get_mut(&3).unwrap().kind = Kind::ApiKey;
        assert_eq!(order(&s, Strategy::Balanced, &[]), vec![4]);
    }

    #[test]
    fn priority_outranks_room() {
        let mut s = state(&[(1, 10.0, 10.0, 100.0), (2, 70.0, 70.0, 100.0)]);
        s.accounts.get_mut(&2).unwrap().priority = 1;
        assert_eq!(order(&s, Strategy::Balanced, &[]), vec![2, 1]);
    }

    #[test]
    fn a_window_past_its_reset_counts_as_empty() {
        let mut s = state(&[(1, 100.0, 10.0, 100.0)]);
        s.usage.get_mut(&1).unwrap().usage.as_mut().unwrap().five_hour = Some(Window {
            pct: Some(100.0),
            resets_at: at(-0.5),
        });
        let r = room(s.usage[&1].usage.as_ref().unwrap(), &s.settings, NOW);
        assert!(!r.at_limit);
        assert_eq!(r.window, "7d");
    }

    #[test]
    fn stays_while_the_active_account_has_room() {
        let s = state(&[(1, 85.0, 50.0, 100.0), (2, 0.0, 0.0, 100.0)]);
        assert!(matches!(decision(&s, 1, 0), Decision::Stay(_)));
    }

    #[test]
    fn switches_when_the_active_account_crosses_its_line() {
        let s = state(&[(1, 92.0, 50.0, 100.0), (2, 30.0, 20.0, 100.0), (3, 10.0, 20.0, 100.0)]);
        assert!(matches!(decision(&s, 1, 0), Decision::Switch { to: 3, .. }));
    }

    #[test]
    fn the_weekly_line_is_separate_from_the_five_hour_line() {
        // 7d at 93 is under its 95 line, so it holds; at 96 it moves.
        let s = state(&[(1, 10.0, 93.0, 100.0), (2, 10.0, 10.0, 100.0)]);
        assert!(matches!(decision(&s, 1, 0), Decision::Stay(_)));
        let s = state(&[(1, 10.0, 96.0, 100.0), (2, 10.0, 10.0, 100.0)]);
        assert!(matches!(decision(&s, 1, 0), Decision::Switch { to: 2, .. }));
    }

    #[test]
    fn cooldown_holds_a_switch_unless_the_account_is_at_its_limit() {
        let recent = NOW - 60_000;
        let s = state(&[(1, 92.0, 50.0, 100.0), (2, 10.0, 10.0, 100.0)]);
        assert!(matches!(decision(&s, 1, recent), Decision::Stay(_)));
        let s = state(&[(1, 100.0, 50.0, 100.0), (2, 10.0, 10.0, 100.0)]);
        assert!(matches!(decision(&s, 1, recent), Decision::Switch { to: 2, .. }));
    }

    #[test]
    fn does_not_ping_pong_between_two_accounts_near_the_line() {
        // #1 is 1 point over; #2 has 4 points of room: a gain of 5, under the margin of 10.
        let s = state(&[(1, 91.0, 50.0, 100.0), (2, 86.0, 50.0, 100.0)]);
        assert!(matches!(decision(&s, 1, 0), Decision::Stay(_)));
    }

    #[test]
    fn holds_when_every_account_is_exhausted() {
        let s = state(&[(1, 100.0, 50.0, 100.0), (2, 100.0, 50.0, 100.0)]);
        assert!(matches!(decision(&s, 1, 0), Decision::Stay(_)));
    }

    #[test]
    fn leaves_an_exhausted_account_for_one_that_is_merely_over_its_line() {
        let s = state(&[(1, 100.0, 50.0, 100.0), (2, 93.0, 50.0, 100.0)]);
        assert!(matches!(decision(&s, 1, 0), Decision::Switch { to: 2, .. }));
    }

    #[test]
    fn leaves_an_account_that_needs_sign_in() {
        let mut s = state(&[(1, 10.0, 10.0, 100.0), (2, 40.0, 40.0, 100.0)]);
        s.accounts.get_mut(&1).unwrap().needs_login = true;
        assert!(matches!(decision(&s, 1, NOW - 1000), Decision::Switch { to: 2, .. }));
    }

    #[test]
    fn drain_soonest_moves_early_towards_quota_about_to_lapse() {
        let mut s = state(&[(1, 10.0, 10.0, 120.0), (2, 10.0, 30.0, 8.0)]);
        s.settings.strategy = Strategy::DrainSoonest;
        assert!(matches!(decision(&s, 1, 0), Decision::Switch { to: 2, .. }));
        s.settings.strategy = Strategy::Balanced;
        assert!(matches!(decision(&s, 1, 0), Decision::Stay(_)));
    }

    #[test]
    fn an_unmanaged_default_login_is_left_alone() {
        let s = state(&[(1, 10.0, 10.0, 100.0)]);
        let v = evaluate(&s, &BTreeMap::new(), Strategy::Balanced, NOW);
        assert!(matches!(decide(None, &v, &s.settings, NOW, 0), Decision::Stay(_)));
    }
}
