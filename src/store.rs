//! cx's own state: the account list, settings, the last usage readings and
//! the map from token fingerprint to account. Secrets are not in this file;
//! they live in the Keychain (see `secrets`).
//!
//! Several cx processes can run at once (the dashboard, `cx auto`, a one-off
//! command), so every change goes through `Store::update`, which takes the
//! state lock, re-reads the file, applies the change and writes it back.

use crate::lock::DirLock;
use crate::util;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// What sort of credential an account holds. It decides how cx signs in,
/// whether usage can be read and how use is billed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// A Claude subscription login with a refresh token.
    Oauth,
    /// A token from `claude setup-token`: inference only, no usage readings.
    SetupToken,
    /// A Console API key, billed per token.
    ApiKey,
}

/// One Claude account that cx manages, as stored in `state.json`. The
/// credential itself is in the Keychain under [`Account::secret_name`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    /// The account's number, as shown on screen (`#3`) and typed by the user.
    /// A removed account's number is given to the next account added.
    pub id: u32,
    /// What sort of credential the account holds.
    pub kind: Kind,
    /// Email for a login; a label the user chose for a key.
    pub label: String,
    /// A short name the user chose with `cx alias`. It is shown in place of
    /// the label and is unique across accounts. `None` when no alias is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// The user's account UUID on the server. Empty for a key, and for a
    /// login whose profile has not been read yet.
    #[serde(default)]
    pub uuid: String,
    /// The UUID of the organization the login belongs to. Empty when unknown.
    #[serde(default)]
    pub org_uuid: String,
    /// The organization's name, for display. Empty when unknown.
    #[serde(default)]
    pub org_name: String,
    /// The subscription plan as shown on screen, such as "Max 20x". `None`
    /// until the profile has been read, and for a key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// When the plan was last read from the server, in milliseconds since
    /// the Unix epoch. 0 means it has never been read.
    #[serde(default)]
    pub plan_checked_ms: i64,
    /// Held out of automatic selection. Still usable by name.
    #[serde(default)]
    pub disabled: bool,
    /// Higher is preferred by every strategy.
    #[serde(default)]
    pub priority: i32,
    /// The account's own switch line, in percent, set with `cx threshold`.
    /// It replaces both `threshold_5h` and `threshold_7d` for this account.
    /// `None` uses the settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_line: Option<f64>,
    /// The refresh token was rejected: the account needs `cx login`.
    #[serde(default)]
    pub needs_login: bool,
    /// The `oauthAccount` section Claude Code wants in its config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_account: Option<Value>,
    /// When the account was added to cx, in milliseconds since the Unix
    /// epoch.
    #[serde(default)]
    pub added_ms: i64,
    /// When the stored access token expires. Kept here so a listing does not
    /// have to open the Keychain.
    #[serde(default)]
    pub token_expires_ms: i64,
    /// The last characters of a key, to tell keys apart on screen.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hint: String,
}

impl Account {
    /// A new account with this number, kind and label. Every other field
    /// starts empty, and `added_ms` is set to the current time.
    pub fn new(id: u32, kind: Kind, label: &str) -> Account {
        Account {
            id,
            kind,
            label: label.to_string(),
            alias: None,
            uuid: String::new(),
            org_uuid: String::new(),
            org_name: String::new(),
            plan: None,
            plan_checked_ms: 0,
            disabled: false,
            priority: 0,
            switch_line: None,
            needs_login: false,
            oauth_account: None,
            added_ms: crate::util::now_ms(),
            token_expires_ms: 0,
            hint: String::new(),
        }
    }

    /// The name of the Keychain entry that holds this account's credential:
    /// `acct-<id>`.
    pub fn secret_name(&self) -> String {
        format!("acct-{}", self.id)
    }

    /// The account as shown to the user: `#<id>` followed by the alias when
    /// there is one, else the label.
    pub fn display(&self) -> String {
        match &self.alias {
            Some(a) => format!("#{} {a}", self.id),
            None => format!("#{} {}", self.id, self.label),
        }
    }
}

/// One rate-limit window of an account, as last read from the server.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// Percent of the window used, 0 to 100. `None` when the server sent null.
    pub pct: Option<f64>,
    /// When the window resets, as the RFC 3339 timestamp the server sent.
    /// `None` when the server gave no reset time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
}

impl Window {
    /// Percent used now. A window whose reset time has passed is empty again,
    /// whatever the last reading said.
    pub fn pct_at(&self, now_ms: i64) -> Option<f64> {
        match self.resets_at.as_deref().and_then(util::parse_rfc3339_ms) {
            Some(reset) if reset <= now_ms => Some(0.0),
            _ => self.pct,
        }
    }

    /// Seconds from `now_ms` until the window resets. Negative when the reset
    /// time has passed. `None` when there is no reset time or it does not
    /// parse.
    pub fn resets_in_s(&self, now_ms: i64) -> Option<i64> {
        self.resets_at
            .as_deref()
            .and_then(util::parse_rfc3339_ms)
            .map(|reset| (reset - now_ms) / 1000)
    }
}

/// A weekly limit that applies to one model.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelWindow {
    /// The model's name as the server reports it, such as "Fable".
    pub name: String,
    /// How much of that limit is used and when it resets. In JSON its fields
    /// sit beside `name` and are not nested.
    #[serde(flatten)]
    pub window: Window,
}

/// Pay-as-you-go use beyond the subscription, for an account that has
/// extra usage switched on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExtraUsage {
    /// Amount spent this month, in units of `currency` (dollars, not cents).
    pub used: f64,
    /// The monthly spending limit, in units of `currency`.
    pub limit: f64,
    /// Percent of the monthly limit used, as the server reports it.
    pub pct: f64,
    /// The ISO 4217 currency code, such as "USD".
    pub currency: String,
}

/// One usage reading for an account: every limit the server reported.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// The rolling 5-hour window. `None` when the server did not report one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<Window>,
    /// The weekly window across all models. `None` when the server did not
    /// report one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<Window>,
    /// Weekly limits that apply to one model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelWindow>,
    /// Extra usage spending. `None` when the account has it switched off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<ExtraUsage>,
}

/// What cx remembers about one account's usage between fetches.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UsageRecord {
    /// The last good reading. A failed fetch never clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// When the last good reading was fetched, in milliseconds since the Unix
    /// epoch. 0 means there has never been one.
    #[serde(default)]
    pub fetched_ms: i64,
    /// Why the last fetch failed. `None` when it succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Do not ask the server again before this time.
    #[serde(default)]
    pub retry_after_ms: i64,
    /// When a reading last went into the history file.
    #[serde(default)]
    pub sampled_ms: i64,
}

/// How cx chooses an account when the user does not name one. Written in
/// kebab-case in `state.json` and on the command line.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Most room left, spread across running sessions, with a push towards
    /// weekly quota that is about to reset unused.
    Balanced,
    /// Most room left, nothing else.
    Headroom,
    /// The account whose weekly window resets soonest and still has room.
    DrainSoonest,
}

impl std::str::FromStr for Strategy {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Strategy> {
        match s {
            "balanced" => Ok(Strategy::Balanced),
            "headroom" => Ok(Strategy::Headroom),
            "drain-soonest" => Ok(Strategy::DrainSoonest),
            other => bail!("unknown strategy '{other}' (balanced, headroom, drain-soonest)"),
        }
    }
}

impl std::fmt::Display for Strategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Strategy::Balanced => "balanced",
            Strategy::Headroom => "headroom",
            Strategy::DrainSoonest => "drain-soonest",
        })
    }
}

/// The user's settings, changed with `cx config`. A key missing from
/// `state.json` takes its value from [`Settings::default`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Leave an account when its 5-hour window reaches this percent.
    pub threshold_5h: f64,
    /// Leave an account when its weekly window reaches this percent.
    pub threshold_7d: f64,
    /// A new account must have this many more points of room than the current one.
    pub margin: f64,
    /// Seconds between automatic switches.
    pub cooldown_s: u64,
    /// Seconds between checks in `cx auto`.
    pub interval_s: u64,
    /// How an account is chosen when the user does not name one.
    pub strategy: Strategy,
    /// Seconds a usage reading stays fresh.
    pub usage_ttl_s: u64,
    /// Which per-model weekly limits count when judging an account: "auto"
    /// (the default, the model Claude Code used last; see
    /// [`Settings::settled_model`]), "all", one model's name such as "Fable",
    /// or unset for none.
    pub model: Option<String>,
    /// Let `cx run` fall back to an API key when every subscription is at
    /// its limit. API keys are billed per token, so this is off by default.
    pub api_fallback: bool,
    /// Show a desktop notification when `cx auto` switches, when an account
    /// needs a new sign-in and when every account is at its limit.
    pub notify: bool,
    /// Keep the Mac awake for a while after a turn ends on a large
    /// conversation, so Claude Code can summarise it before its cache
    /// expires. Needs the cx plugin's hooks. Off by default: it uses power.
    #[serde(default)]
    pub keep_awake: bool,
    /// Entries of `~/.claude` that a `cx run` profile shares by symlink.
    pub share: Vec<String>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            threshold_5h: 90.0,
            threshold_7d: 95.0,
            margin: 10.0,
            cooldown_s: 300,
            interval_s: 60,
            strategy: Strategy::Balanced,
            usage_ttl_s: 120,
            model: Some("auto".into()),
            api_fallback: false,
            notify: true,
            keep_awake: false,
            share: [
                "settings.json",
                "keybindings.json",
                "CLAUDE.md",
                "skills",
                "commands",
                "agents",
                "rules",
                "hooks",
                "plugins",
                "output-styles",
                "projects",
                "history.jsonl",
                "file-history",
                "plans",
                "todos",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        }
    }
}

/// Every setting with a line on what it does, in the order they are shown.
pub const SETTING_HELP: [(&str, &str); 12] = [
    (
        "strategy",
        "How a bare `cx run` and `cx use` choose: balanced, headroom or drain-soonest",
    ),
    (
        "threshold_5h",
        "Leave an account when its 5-hour window reaches this percent",
    ),
    (
        "threshold_7d",
        "Leave an account when a weekly window reaches this percent",
    ),
    (
        "model",
        "Which per-model weekly limit counts: auto (the model in use), all, a model name, or none",
    ),
    ("margin", "Points of room a new account must have over the current one"),
    ("cooldown_s", "Seconds between automatic switches"),
    ("interval_s", "Seconds between checks in `cx auto`"),
    (
        "usage_ttl_s",
        "Seconds a usage reading stays fresh for the active account",
    ),
    (
        "api_fallback",
        "Let a bare `cx run` use an API key when every subscription is at its limit",
    ),
    ("notify", "Desktop notifications from `cx auto`"),
    (
        "keep_awake",
        "Keep the Mac awake after a turn on a large conversation, so it is summarised before its cache expires",
    ),
    (
        "share",
        "Entries of ~/.claude that a `cx run` profile shares by symlink",
    ),
];

/// Is this a single file or directory name: not empty, not `.` or `..`,
/// and with no path separator in it?
pub fn plain_name(name: &str) -> bool {
    let mut parts = std::path::Path::new(name).components();
    matches!(
        (parts.next(), parts.next()),
        (Some(std::path::Component::Normal(only)), None) if only == std::ffi::OsStr::new(name)
    ) && !name.contains(['/', '\\', '\0'])
}

fn show_value(v: &Value) -> String {
    match v {
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(","),
        Value::Null => "none".into(),
        Value::String(s) => s.clone(),
        // 90.0 is shown as 90.
        Value::Number(n) if n.as_f64().is_some_and(|f| f.fract() == 0.0) => format!("{}", n.as_f64().unwrap()),
        other => other.to_string(),
    }
}

impl Settings {
    /// A setting's value as text, or `None` for an unknown key.
    pub fn show(&self, key: &str) -> Option<String> {
        serde_json::to_value(self).ok()?.get(key).map(show_value)
    }

    /// Whether the setting `key` still has its default value. Two unknown
    /// keys compare equal, so an unknown key also returns true.
    pub fn is_default(&self, key: &str) -> bool {
        Settings::default().show(key) == self.show(key)
    }

    /// Check every value, whatever its source: typed by the user, read from
    /// an import file, or loaded from disk.
    pub fn validate(&self) -> Result<()> {
        for (key, _) in SETTING_HELP {
            if let Some((_, low, high)) = Settings::number_range(key)
                && let Some(number) = self.show(key).and_then(|v| v.parse::<f64>().ok())
                && !(low..=high).contains(&number)
            {
                bail!("{key} must be between {low} and {high}");
            }
        }
        // A shared entry becomes a symlink inside a profile directory, so it
        // must name something directly inside `~/.claude` and nothing else.
        if let Some(bad) = self.share.iter().find(|name| !plain_name(name)) {
            bail!("share entry '{bad}' must be a plain file or directory name, without a path");
        }
        Ok(())
    }

    /// Set one setting from text, checking it.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let mut settings = serde_json::to_value(&*self)?;
        let defaults = serde_json::to_value(Settings::default())?;
        if settings.get(key).is_none() {
            bail!("unknown setting '{key}'. `cx config` lists them.");
        }
        let parsed = match defaults.get(key) {
            Some(Value::Array(_)) => serde_json::json!(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            ),
            Some(Value::Bool(_)) => match value {
                "true" | "on" | "yes" | "1" => Value::Bool(true),
                "false" | "off" | "no" | "0" => Value::Bool(false),
                _ => bail!("{key} is true or false"),
            },
            Some(Value::Number(n)) => {
                let number = value.parse::<f64>().with_context(|| format!("{key} needs a number"))?;
                // Whole numbers are stored as integers, so `60` fits a seconds field.
                if n.is_u64() {
                    if number < 0.0 || number.fract() != 0.0 {
                        bail!("{key} needs a whole number");
                    }
                    serde_json::json!(number as u64)
                } else {
                    serde_json::json!(number)
                }
            }
            _ if matches!(value, "none" | "unset" | "") => Value::Null,
            _ => serde_json::json!(value),
        };
        settings[key] = parsed;
        let settings: Settings =
            serde_json::from_value(settings).with_context(|| format!("'{value}' is not valid for {key}"))?;
        settings.validate()?;
        *self = settings;
        Ok(())
    }

    /// How much one press changes a numeric setting, and its bounds.
    pub fn number_range(key: &str) -> Option<(f64, f64, f64)> {
        Some(match key {
            "threshold_5h" | "threshold_7d" => (1.0, 50.0, 100.0),
            "margin" => (1.0, 0.0, 50.0),
            "cooldown_s" => (30.0, 0.0, 86_400.0),
            "interval_s" => (15.0, 15.0, 3_600.0),
            "usage_ttl_s" => (30.0, 30.0, 3_600.0),
            _ => return None,
        })
    }

    /// The values a setting can take, when it is one of a fixed few.
    pub fn choices(key: &str, models: &[String]) -> Option<Vec<String>> {
        match key {
            "strategy" => Some(vec!["balanced".into(), "headroom".into(), "drain-soonest".into()]),
            "model" => {
                let mut options = vec!["auto".to_string(), "all".into(), "none".into()];
                options.extend(models.iter().cloned());
                Some(options)
            }
            _ => None,
        }
    }

    /// The `model` setting with "auto" settled: the limit of the model
    /// Claude Code used last counts when an account reports one for it,
    /// no per-model limit counts when the model in use has none, and all of
    /// them count while the model in use is unknown.
    pub fn settled_model(&self, working: Option<&str>, limited: &[String]) -> Option<String> {
        match self.model.as_deref() {
            Some(m) if m.eq_ignore_ascii_case("auto") => match working {
                None => Some("all".into()),
                Some(id) => limited
                    .iter()
                    .find(|name| id.to_lowercase().contains(&name.to_lowercase()))
                    .cloned(),
            },
            other => other.map(String::from),
        }
    }

    /// Does this model's weekly limit count when judging an account?
    pub fn counts_model(&self, name: &str) -> bool {
        match self.model.as_deref() {
            None => false,
            Some(wanted) => {
                // "auto" that nobody settled counts everything: the safe side.
                wanted.eq_ignore_ascii_case("all")
                    || wanted.eq_ignore_ascii_case("auto")
                    || wanted.eq_ignore_ascii_case(name)
            }
        }
    }
}

impl State {
    /// The models some account reports a weekly limit for, in first-seen order.
    pub fn model_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for record in self.usage.values() {
            for model in record.usage.iter().flat_map(|u| u.models.iter()) {
                if !names.iter().any(|n| n.eq_ignore_ascii_case(&model.name)) {
                    names.push(model.name.clone());
                }
            }
        }
        names
    }
}

impl Usage {
    /// The weekly limit for one model. The name is matched without regard to
    /// case. `None` when the reading has no limit for that model.
    pub fn model(&self, name: &str) -> Option<&Window> {
        self.models
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case(name))
            .map(|m| &m.window)
    }
}

/// Everything in `state.json`. Read it with [`Store::load`] and change it
/// only through [`Store::update`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// The file format version. It is 1, and [`Store::update`] writes 1.
    pub version: u32,
    /// Every account, by its number.
    pub accounts: BTreeMap<u32, Account>,
    /// Which account a refresh-token fingerprint belongs to. This is how cx
    /// recognises a login it finds in a Claude Code home without asking the
    /// server.
    pub fingerprints: BTreeMap<String, u32>,
    /// The usage record of each account, by account number. An account that
    /// has never been fetched has no entry.
    pub usage: BTreeMap<u32, UsageRecord>,
    /// The user's settings.
    pub settings: Settings,
    /// When `cx auto` last switched accounts, in milliseconds since the Unix
    /// epoch. 0 means never. The cooldown is counted from here.
    pub last_auto_switch_ms: i64,
    /// Directories pinned to an account: `cx run` there starts on it.
    pub mappings: BTreeMap<String, u32>,
}

impl State {
    /// Find an account by number, alias, label, or an unambiguous part of one.
    pub fn resolve(&self, query: &str) -> Result<&Account> {
        let q = query.trim().trim_start_matches('#');
        if let Ok(id) = q.parse::<u32>() {
            return self.accounts.get(&id).with_context(|| format!("no account #{id}"));
        }
        let lower = q.to_lowercase();
        let exact: Vec<&Account> = self
            .accounts
            .values()
            .filter(|a| {
                a.label.to_lowercase() == lower || a.alias.as_deref().is_some_and(|x| x.to_lowercase() == lower)
            })
            .collect();
        if exact.len() == 1 {
            return Ok(exact[0]);
        }
        let partial: Vec<&Account> = self
            .accounts
            .values()
            .filter(|a| {
                a.label.to_lowercase().contains(&lower)
                    || a.alias.as_deref().is_some_and(|x| x.to_lowercase().contains(&lower))
            })
            .collect();
        match partial.len() {
            1 => Ok(partial[0]),
            0 => bail!("no account matches '{query}' (see `cx ls`)"),
            _ => bail!(
                "'{query}' matches several accounts: {}",
                partial.iter().map(|a| a.display()).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// The account pinned to this directory or its nearest pinned parent.
    pub fn mapped(&self, dir: &std::path::Path) -> Option<(&str, &Account)> {
        dir.ancestors().find_map(|parent| {
            let (path, id) = self.mappings.get_key_value(parent.to_str()?)?;
            Some((path.as_str(), self.accounts.get(id)?))
        })
    }

    /// Give an account a short name, or take it away. Returns how the
    /// account is now shown.
    pub fn set_alias(&mut self, id: u32, name: Option<&str>) -> Result<String> {
        if let Some(name) = name {
            if name.parse::<u32>().is_ok() {
                bail!("an alias cannot be a number");
            }
            if self
                .accounts
                .values()
                .any(|a| a.id != id && a.alias.as_deref() == Some(name))
            {
                bail!("'{name}' is already another account's alias");
            }
        }
        let account = self
            .accounts
            .get_mut(&id)
            .with_context(|| format!("no account #{id}"))?;
        account.alias = name.map(String::from);
        Ok(account.display())
    }

    /// The number the next new account gets: the lowest one not in use, so a
    /// removed account's number is used again.
    pub fn next_id(&self) -> u32 {
        (1..).find(|id| !self.accounts.contains_key(id)).unwrap()
    }

    /// The account holding this identity. An organization is part of the
    /// identity: one person can hold seats in two.
    pub fn find_identity(&self, uuid: &str, org_uuid: &str) -> Option<u32> {
        self.accounts
            .values()
            .find(|a| {
                a.kind == Kind::Oauth
                    && !a.uuid.is_empty()
                    && a.uuid == uuid
                    && (a.org_uuid.is_empty() || org_uuid.is_empty() || a.org_uuid == org_uuid)
            })
            .map(|a| a.id)
    }

    /// Record that the refresh token with this fingerprint belongs to
    /// `account`. Once the map holds more than 400 entries, entries that point
    /// at accounts no longer in the list are dropped.
    pub fn remember_fingerprint(&mut self, fingerprint: String, account: u32) {
        self.fingerprints.insert(fingerprint, account);
        // A refresh retires a fingerprint. Keep a bounded tail per account so
        // a login Claude Code refreshed a while ago is still recognised.
        const KEEP: usize = 400;
        if self.fingerprints.len() > KEEP {
            let live: std::collections::BTreeSet<u32> = self.accounts.keys().copied().collect();
            self.fingerprints.retain(|_, id| live.contains(id));
        }
    }
}

/// A handle on cx's state directory. It holds no state in memory: every
/// read goes to the file.
pub struct Store {
    /// The state directory, as given by [`util::store_root`].
    pub root: PathBuf,
}

impl Store {
    /// Open the state directory, creating it with mode 0700 when it is
    /// missing. Fails when the directory cannot be created.
    pub fn open() -> Result<Store> {
        let root = util::store_root();
        util::mkdir_private(&root)?;
        Ok(Store { root })
    }

    fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }

    /// Read `state.json` without taking a lock. A missing or empty file gives
    /// an empty state. Fails when the file is not JSON or does not have the
    /// shape of a [`State`].
    pub fn load(&self) -> Result<State> {
        match util::read_json(&self.state_path())? {
            Some(value) => serde_json::from_value(value).context("state.json has an unexpected shape"),
            None => Ok(State {
                version: 1,
                ..State::default()
            }),
        }
    }

    /// The lock every operation that moves a credential holds from start to
    /// finish: a switch, a refresh, a sync. One at a time across all cx
    /// processes.
    pub fn lock_operations(&self) -> Result<DirLock> {
        DirLock::acquire(
            &self.root.join("locks").join("operations.lock"),
            Duration::from_secs(60),
            Duration::from_secs(45),
        )
        .context("another cx process is busy with the accounts")
    }

    /// Change the state: take the state lock, read the file, run `change` on
    /// it and write the result back with mode 0600. Returns what `change`
    /// returned. When `change` fails nothing is written. Fails when the lock
    /// is still held by another process after 10 seconds.
    pub fn update<T>(&self, change: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let _lock = DirLock::acquire(
            &self.root.join("locks").join("state.lock"),
            Duration::from_secs(15),
            Duration::from_secs(10),
        )?;
        let mut state = self.load()?;
        let out = change(&mut state)?;
        state.version = 1;
        util::write_json(&self.state_path(), &serde_json::to_value(&state)?, 0o600)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: u32, label: &str, alias: Option<&str>) -> Account {
        let mut a = Account::new(id, Kind::Oauth, label);
        a.alias = alias.map(String::from);
        a.uuid = format!("u{id}");
        a.org_uuid = format!("o{id}");
        a
    }

    fn state() -> State {
        let mut s = State::default();
        for a in [
            account(1, "anna@work.example", Some("work")),
            account(2, "anna@home.example", None),
            account(4, "bo@home.example", None),
        ] {
            s.accounts.insert(a.id, a);
        }
        s
    }

    #[test]
    fn resolves_by_number_alias_label_and_fragment() {
        let s = state();
        assert_eq!(s.resolve("2").unwrap().id, 2);
        assert_eq!(s.resolve("#4").unwrap().id, 4);
        assert_eq!(s.resolve("WORK").unwrap().id, 1);
        assert_eq!(s.resolve("bo@home.example").unwrap().id, 4);
        assert_eq!(s.resolve("bo").unwrap().id, 4);
        assert!(s.resolve("anna").is_err());
        assert!(s.resolve("9").is_err());
    }

    #[test]
    fn a_directory_takes_the_nearest_pin() {
        let mut s = state();
        s.mappings.insert("/work".into(), 1);
        s.mappings.insert("/work/client".into(), 4);
        s.mappings.insert("/gone".into(), 9);
        let at = |p: &str| s.mapped(std::path::Path::new(p)).map(|(_, a)| a.id);
        assert_eq!(at("/work/app/src"), Some(1));
        assert_eq!(at("/work/client/api"), Some(4));
        assert_eq!(at("/elsewhere"), None);
        assert_eq!(at("/gone/x"), None);
    }

    #[test]
    fn settings_are_set_from_text_and_checked() {
        let mut s = Settings::default();
        s.set("threshold_5h", "80").unwrap();
        s.set("cooldown_s", "120").unwrap();
        s.set("strategy", "headroom").unwrap();
        s.set("model", "none").unwrap();
        s.set("notify", "off").unwrap();
        assert_eq!(
            (s.threshold_5h, s.cooldown_s, s.strategy),
            (80.0, 120, Strategy::Headroom)
        );
        assert_eq!((s.model.clone(), s.notify), (None, false));
        assert!(s.set("threshold_5h", "20").is_err());
        assert!(s.set("cooldown_s", "1.5").is_err());
        assert!(s.set("strategy", "random").is_err());
        assert!(s.set("nope", "1").is_err());
        assert!(s.set("notify", "maybe").is_err());
        assert!(s.set("cooldown_s", "99999999").is_err());
        for bad in ["../../.ssh", "/etc", "a/b", "..", ".", "skills,../x"] {
            assert!(s.set("share", bad).is_err(), "{bad:?} was accepted");
        }
        s.set("share", "settings.json,skills").unwrap();
        assert_eq!(s.share, vec!["settings.json", "skills"]);
        assert!(plain_name("history.jsonl") && !plain_name("") && !plain_name("a\\b"));
        let mut crafted = Settings::default();
        crafted.share.push("../outside".into());
        crafted.threshold_5h = 5.0;
        assert!(crafted.validate().is_err());
        assert!(Settings::default().validate().is_ok());
        assert!(s.set("interval_s", "999999").is_err());
        assert_eq!(s.show("share").unwrap().split(',').next(), Some("settings.json"));
        for (key, _) in SETTING_HELP {
            assert!(s.show(key).is_some(), "{key}");
            // Every setting has an editor: a number, a fixed few, on/off, or the list.
            let on_off = matches!(s.show(key).as_deref(), Some("true" | "false"));
            assert!(
                Settings::number_range(key).is_some()
                    || Settings::choices(key, &[]).is_some()
                    || on_off
                    || key == "share",
                "{key} has no editor"
            );
        }
    }

    #[test]
    fn auto_counts_only_the_limit_of_the_model_in_use() {
        let s = Settings::default();
        let limited = vec!["Fable".to_string()];
        assert_eq!(
            s.settled_model(Some("claude-fable-5-2"), &limited).as_deref(),
            Some("Fable")
        );
        assert_eq!(s.settled_model(Some("claude-opus-5-5"), &limited), None);
        assert_eq!(s.settled_model(None, &limited).as_deref(), Some("all"));
        let fixed = Settings {
            model: Some("all".into()),
            ..Settings::default()
        };
        assert_eq!(
            fixed.settled_model(Some("claude-opus-5-5"), &limited).as_deref(),
            Some("all")
        );
    }

    #[test]
    fn next_id_fills_the_first_gap() {
        assert_eq!(state().next_id(), 3);
    }

    #[test]
    fn identity_includes_the_organization() {
        let s = state();
        assert_eq!(s.find_identity("u2", "o2"), Some(2));
        assert_eq!(s.find_identity("u2", "other-org"), None);
        assert_eq!(s.find_identity("u2", ""), Some(2));
    }

    #[test]
    fn a_window_past_its_reset_reads_empty() {
        let w = Window {
            pct: Some(97.0),
            resets_at: Some("2026-01-01T00:00:00Z".into()),
        };
        let reset = util::parse_rfc3339_ms("2026-01-01T00:00:00Z").unwrap();
        assert_eq!(w.pct_at(reset - 1000), Some(97.0));
        assert_eq!(w.pct_at(reset + 1000), Some(0.0));
    }
}
