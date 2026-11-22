//! Keeping logins alive and moving them: recognising the default login,
//! converging the copies of an account's login, refreshing a token, switching
//! the default login, and reading usage.
//!
//! # Where a login lives
//!
//! An account's login can sit in up to three places: cx's own secret for the
//! account; the default Claude Code home, while the account is the default
//! login; and the account's `cx run` profile, once one exists. Claude Code
//! refreshes the copy it is using, and a refresh retires the refresh token
//! in every other copy. So before cx uses an account it **converges** the
//! copies: it reads all of them, takes the one that expires last (the one
//! refreshed last), and writes it over the others.
//!
//! # Rules
//!
//! - One credential operation at a time across all cx processes
//!   (`Store::lock_operations`).
//! - Lock order: cx's operation lock, then a home's credential locks, then
//!   its config lock. cx's state lock is innermost and never held across any
//!   other lock or a network call.
//! - No network call under a home's locks, except the refresh POST itself.
//!   Claude Code makes the same exception: its refresh also holds them.
//! - A token reply is written to cx's secret before anything else. The old
//!   refresh token is already spent, so the reply must not be lost.
//! - A secret overwritten by a different login is kept as `<name>.prev`.

use crate::api::{Api, ApiError, Identity};
use crate::claude::{self, ClaudeHome};
use crate::cred::Oauth;
use crate::history::{self, History, Sample};
use crate::rank::{self, Decision, Verdict};
use crate::secrets;
use crate::store::{Account, Kind, State, Store, Strategy, UsageRecord};
use crate::util::{self, now_ms};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

const HOME_LOCK_WAIT: Duration = Duration::from_secs(9);
const PLAN_TTL_MS: i64 = 6 * 3600 * 1000;
const RATE_LIMIT_PAUSE_MS: i64 = 5 * 60 * 1000;
const FAILURE_PAUSE_MS: i64 = 60 * 1000;

/// The operations behind every cx command: it owns the on-disk store and the
/// HTTP client, and applies the credential rules in the module header.
pub struct Engine {
    /// cx's state file, its locks and its run directory.
    pub store: Store,
    /// Client for the token, profile and usage endpoints.
    pub api: Api,
    /// The model Claude Code used last, and when cx looked.
    working: std::sync::Mutex<Option<(std::time::Instant, Option<String>)>>,
}

/// What the default Claude Code home holds right now.
#[derive(Clone, Debug, Default)]
pub struct Live {
    /// The login found there. `None` when Claude Code is signed out.
    pub oauth: Option<Oauth>,
    /// The cx account this login belongs to, when it is one.
    pub account: Option<u32>,
    /// The server confirmed the owner, or cx wrote this exact login itself.
    pub verified: bool,
}

/// The result of [`Engine::switch_to`].
#[derive(Debug)]
pub struct Switched {
    /// Number of the account that is now the default login.
    pub to: u32,
    /// False when that account was already the default login.
    pub changed: bool,
    /// Name of the secret holding a default login cx did not recognise and
    /// put aside before overwriting it.
    pub stashed: Option<String>,
}

/// Everything a listing or the dashboard shows, read in one pass.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// cx's stored state, including the usage readings just taken.
    pub state: State,
    /// The account that is the default login. `None` when Claude Code is
    /// signed out or holds a login cx does not manage.
    pub live: Option<u32>,
    /// The default home holds a login that is not one of cx's accounts.
    pub live_unmanaged: bool,
    /// How many `cx run` sessions are alive, by account number.
    pub sessions: BTreeMap<u32, usize>,
    /// Every account judged under the stored strategy, best first.
    pub verdicts: Vec<Verdict>,
    /// When the snapshot was taken, in milliseconds since the Unix epoch.
    pub now_ms: i64,
    /// The model Claude Code used last, as its id.
    pub working_model: Option<String>,
    /// The `model` setting as stored. `state.settings.model` holds it with
    /// "auto" settled, which is what the ranking reads.
    pub model_setting: Option<String>,
}

impl Snapshot {
    /// The verdict on one account. `None` when there is no such account.
    pub fn verdict(&self, id: u32) -> Option<&Verdict> {
        self.verdicts.iter().find(|v| v.id == id)
    }
}

/// One live `cx run` session, as recorded in cx's run directory.
#[derive(Clone, Debug)]
pub struct RunFile {
    /// Process id of the session. Claude Code keeps the pid `cx run` had.
    pub pid: u32,
    /// Number of the account the session runs as.
    pub account: u32,
}

impl Engine {
    /// Open cx's store and build the HTTP client. Fails when the store
    /// cannot be opened. Makes no network call.
    pub fn open() -> Result<Engine> {
        Ok(Engine {
            store: Store::open()?,
            api: Api::new(),
            working: std::sync::Mutex::new(None),
        })
    }

    /// The model Claude Code used last, looked up at most twice a minute.
    pub fn working_model(&self) -> Option<String> {
        let mut cached = self.working.lock().unwrap();
        if let Some((at, model)) = cached.as_ref()
            && at.elapsed() < Duration::from_secs(30)
        {
            return model.clone();
        }
        let model = crate::activity::latest_model_quick(&ClaudeHome::default_home().dir);
        *cached = Some((std::time::Instant::now(), model.clone()));
        model
    }

    // ---- secrets -------------------------------------------------------

    /// Read an account's login from cx's secret store (the macOS Keychain or
    /// the file backend). `None` when nothing is stored or the stored text
    /// is not an OAuth login, as for an API key or a setup-token.
    pub fn read_secret(&self, account: &Account) -> Result<Option<Oauth>> {
        Ok(secrets::get(&account.secret_name())?.and_then(|t| Oauth::from_credential(&t)))
    }

    /// Store an account's login and record whose it is.
    pub fn write_secret(&self, id: u32, oauth: &Oauth) -> Result<()> {
        let name = format!("acct-{id}");
        if let Some(previous) = secrets::get(&name)? {
            let replaced = Oauth::from_credential(&previous).is_none_or(|p| p.refresh() != oauth.refresh());
            if replaced {
                let _ = secrets::set(&format!("{name}.prev"), &previous);
            }
        }
        secrets::set(&name, &oauth.to_credential())?;
        self.store.update(|state| {
            state.remember_fingerprint(oauth.fingerprint(), id);
            if let Some(account) = state.accounts.get_mut(&id) {
                account.token_expires_ms = oauth.expires_ms();
            }
            Ok(())
        })
    }

    // ---- the default login ---------------------------------------------

    /// Work out whose login the default home holds.
    ///
    /// A fingerprint cx has seen settles it without a network call. An
    /// unknown login with a working access token is looked up with the
    /// profile endpoint. Failing both, the `oauthAccount` section of Claude
    /// Code's config is taken as a hint and the result is marked unverified.
    pub fn identify_live(&self, network: bool) -> Result<Live> {
        let home = ClaudeHome::default_home();
        let Some(oauth) = home.read_oauth()? else {
            return Ok(Live::default());
        };
        let state = self.store.load()?;
        if let Some(id) = state.fingerprints.get(&oauth.fingerprint())
            && state.accounts.contains_key(id)
        {
            return Ok(Live {
                oauth: Some(oauth),
                account: Some(*id),
                verified: true,
            });
        }
        if network
            && oauth.expires_ms() > now_ms()
            && let Ok(profile) = self.api.profile(oauth.access())
        {
            let found = state.find_identity(
                &profile.identity.uuid,
                profile.identity.org_uuid.as_deref().unwrap_or(""),
            );
            if let Some(id) = found {
                self.store.update(|s| {
                    s.remember_fingerprint(oauth.fingerprint(), id);
                    Ok(())
                })?;
            }
            return Ok(Live {
                oauth: Some(oauth),
                account: found,
                verified: true,
            });
        }
        let hinted = home
            .oauth_account()
            .as_ref()
            .and_then(claude::identity_of)
            .and_then(|(uuid, org)| state.find_identity(&uuid, &org));
        Ok(Live {
            oauth: Some(oauth),
            account: hinted,
            verified: false,
        })
    }

    // ---- convergence ---------------------------------------------------

    /// The Claude Code homes that hold this account's login.
    fn homes_of(&self, account: &Account, live: &Live, state: &State) -> Vec<ClaudeHome> {
        let mut homes = Vec::new();
        if live.account == Some(account.id) {
            homes.push(ClaudeHome::default_home());
        }
        let profile = ClaudeHome::profile(account.id);
        if profile.exists() && self.profile_belongs(&profile, account, state) {
            homes.push(profile);
        }
        homes
    }

    /// A profile is created for one account, but nothing stops a user from
    /// running `/login` inside it. Its login counts as the account's only
    /// while its config still names the account, or cx knows the fingerprint.
    fn profile_belongs(&self, profile: &ClaudeHome, account: &Account, state: &State) -> bool {
        match profile.oauth_account().as_ref().and_then(claude::identity_of) {
            Some((uuid, _)) => uuid == account.uuid,
            None => match profile.read_oauth() {
                Ok(Some(oauth)) => state.fingerprints.get(&oauth.fingerprint()) == Some(&account.id),
                _ => true,
            },
        }
    }

    fn freshest(&self, account: &Account, homes: &[ClaudeHome]) -> Result<(Option<Oauth>, Vec<Option<Oauth>>)> {
        let stored = self.read_secret(account)?;
        let in_homes: Vec<Option<Oauth>> = homes.iter().map(|h| h.read_oauth().ok().flatten()).collect();
        let best = std::iter::once(stored.clone())
            .chain(in_homes.iter().cloned())
            .flatten()
            .max_by_key(|o| o.expires_ms());
        Ok((best, std::iter::once(stored).chain(in_homes).collect()))
    }

    /// Make every copy of the account's login the freshest one, and return it.
    pub fn converge(&self, id: u32, live: &Live) -> Result<Option<Oauth>> {
        let state = self.store.load()?;
        let account = state.accounts.get(&id).with_context(|| format!("no account #{id}"))?;
        if account.kind != Kind::Oauth {
            return Ok(None);
        }
        let homes = self.homes_of(account, live, &state);
        let (best, copies) = self.freshest(account, &homes)?;
        let Some(best) = best else { return Ok(None) };
        if copies.iter().all(|c| c.as_ref().is_some_and(|c| c.same_tokens(&best))) {
            if account.token_expires_ms != best.expires_ms() {
                self.write_secret(id, &best)?;
            }
            return Ok(Some(best));
        }
        let _operation = self.store.lock_operations()?;
        self.converge_locked(account, &homes).map(Some)
    }

    /// With the operation lock held: take each home's locks, read again, and
    /// write the freshest copy everywhere. A home whose locks cannot be had
    /// is left for the next pass; Claude Code is busy refreshing it.
    fn converge_locked(&self, account: &Account, homes: &[ClaudeHome]) -> Result<Oauth> {
        let mut locked = Vec::new();
        for home in homes {
            if let Ok(locks) = home.lock_credentials(HOME_LOCK_WAIT) {
                locked.push((home.clone(), locks));
            }
        }
        let held: Vec<ClaudeHome> = locked.iter().map(|(h, _)| h.clone()).collect();
        let (best, copies) = self.freshest(account, &held)?;
        let best = best.ok_or_else(|| anyhow!("{} has no stored login", account.display()))?;
        self.write_everywhere(account.id, &best, &held, &copies)?;
        Ok(best)
    }

    fn write_everywhere(&self, id: u32, oauth: &Oauth, homes: &[ClaudeHome], copies: &[Option<Oauth>]) -> Result<()> {
        if !copies
            .first()
            .and_then(Option::as_ref)
            .is_some_and(|c| c.same_tokens(oauth))
        {
            self.write_secret(id, oauth)?;
        }
        for (home, copy) in homes.iter().zip(copies.iter().skip(1)) {
            if !copy.as_ref().is_some_and(|c| c.same_tokens(oauth)) {
                home.write_oauth(oauth)?;
            }
        }
        Ok(())
    }

    // ---- refresh -------------------------------------------------------

    /// Return a login for the account whose access token works, refreshing
    /// it when it has expired or `force` is set.
    pub fn ensure_fresh(&self, id: u32, live: &Live, force: bool) -> Result<Oauth> {
        let converged = self.converge(id, live)?;
        let state = self.store.load()?;
        let account = state.accounts.get(&id).with_context(|| format!("no account #{id}"))?;
        let Some(current) = converged else {
            bail!("{} has no stored login. Run `cx login {id}`.", account.display());
        };
        if !force && current.usable_at(now_ms()) {
            return Ok(current);
        }
        if current.refresh().is_none() {
            bail!("{} has no refresh token. Run `cx login {id}`.", account.display());
        }

        let _operation = self.store.lock_operations()?;
        let homes = self.homes_of(account, live, &state);
        let mut locks = Vec::new();
        for home in &homes {
            locks.push(home.lock_credentials(HOME_LOCK_WAIT)?);
        }
        // Another process, or Claude Code itself, may have refreshed while
        // this one waited for the locks.
        let (best, copies) = self.freshest(account, &homes)?;
        let mut oauth = best.ok_or_else(|| anyhow!("{} has no stored login", account.display()))?;
        if !force && oauth.usable_at(now_ms()) {
            self.write_everywhere(id, &oauth, &homes, &copies)?;
            return Ok(oauth);
        }
        let refresh_token = oauth.refresh().context("no refresh token")?.to_string();
        match self.api.refresh(&refresh_token) {
            Ok(reply) => {
                oauth.apply_refresh(
                    &reply.access_token,
                    reply.refresh_token.as_deref(),
                    reply.expires_in_s,
                    reply.scope.as_deref(),
                    now_ms(),
                );
                self.write_secret(id, &oauth)?;
                for home in &homes {
                    home.write_oauth(&oauth)?;
                }
                let stranger = reply
                    .identity
                    .as_ref()
                    .is_some_and(|i| !account.uuid.is_empty() && i.uuid != account.uuid);
                self.store.update(|s| {
                    if let Some(a) = s.accounts.get_mut(&id) {
                        a.needs_login = false;
                    }
                    Ok(())
                })?;
                if stranger {
                    bail!(
                        "the login stored for {} belongs to a different account. Run `cx login {id}` to replace it.",
                        account.display()
                    );
                }
                Ok(oauth)
            }
            Err(ApiError::Rejected(_)) => {
                self.store.update(|s| {
                    if let Some(a) = s.accounts.get_mut(&id) {
                        a.needs_login = true;
                    }
                    Ok(())
                })?;
                bail!(
                    "{}: the refresh token is no longer valid. Run `cx login {id}`.",
                    account.display()
                )
            }
            Err(ApiError::UnusableTokenReply(body)) => {
                let name = format!("{}.reply-{}", account.secret_name(), now_ms());
                secrets::set(&name, &body)?;
                bail!(
                    "{}: the token endpoint answered in a shape cx does not know. Its reply is kept in the Keychain as '{name}' (service '{}') so the new token is not lost. Please report this.",
                    account.display(),
                    secrets::SERVICE
                )
            }
            Err(e) => bail!("{}: could not refresh the token ({e})", account.display()),
        }
    }

    // ---- switch --------------------------------------------------------

    /// Make an account the default Claude Code login.
    pub fn switch_to(&self, id: u32) -> Result<Switched> {
        let state = self.store.load()?;
        let target = state
            .accounts
            .get(&id)
            .with_context(|| format!("no account #{id}"))?
            .clone();
        if target.kind != Kind::Oauth {
            bail!(
                "{} is not a subscription login, so it cannot be the default login. Use `cx run {id}` to start Claude Code with it.",
                target.display()
            );
        }
        let live = self.identify_live(true)?;
        let home = ClaudeHome::default_home();

        if live.account == Some(id) {
            self.ensure_fresh(id, &live, false)?;
            self.write_oauth_account(&home, &target)?;
            return Ok(Switched {
                to: id,
                changed: false,
                stashed: None,
            });
        }

        // Network work first: a working token for the target, and the
        // outgoing account's rotated token brought home.
        self.ensure_fresh(id, &Live::default(), false)?;
        if let Some(outgoing) = live.account {
            let _ = self.converge(outgoing, &live);
        }

        let _operation = self.store.lock_operations()?;
        let _locks = home.lock_credentials(HOME_LOCK_WAIT)?;
        let now_live = home.read_oauth()?;
        let mut stashed = None;
        if let Some(current) = &now_live {
            match live.account {
                // Claude Code may have refreshed between the check and the lock.
                Some(outgoing) => {
                    let known =
                        self.read_secret(state.accounts.get(&outgoing).context("outgoing account vanished")?)?;
                    if known.is_none_or(|k| current.expires_ms() > k.expires_ms()) {
                        self.write_secret(outgoing, current)?;
                    }
                }
                // A login cx does not manage. It may be the only copy of its
                // refresh token, so it is put aside, never dropped.
                None if current.refresh().is_some() => {
                    let name = format!("stash-{}", now_ms());
                    secrets::set(&name, &current.to_credential())?;
                    stashed = Some(name);
                }
                None => {}
            }
        }
        let incoming = self
            .read_secret(&target)?
            .with_context(|| format!("{} has no stored login", target.display()))?;
        home.write_oauth(&incoming)?;
        if let Err(e) = self.write_oauth_account(&home, &target) {
            // Put the old login back so the home is not half switched.
            if let Some(previous) = &now_live {
                let _ = home.write_oauth(previous);
            }
            return Err(e.context("could not update Claude Code's config; the switch was undone"));
        }
        self.store.update(|s| {
            s.remember_fingerprint(incoming.fingerprint(), id);
            Ok(())
        })?;
        Ok(Switched {
            to: id,
            changed: true,
            stashed,
        })
    }

    fn write_oauth_account(&self, home: &ClaudeHome, account: &Account) -> Result<()> {
        let wanted = account.oauth_account.clone().unwrap_or_else(|| {
            json!({
                "accountUuid": account.uuid,
                "emailAddress": account.label,
                "organizationUuid": account.org_uuid,
                "organizationName": account.org_name,
            })
        });
        if home.oauth_account().as_ref() == Some(&wanted) {
            return Ok(());
        }
        home.update_config(|config| {
            config.insert("oauthAccount".into(), wanted);
        })
    }

    // ---- usage ---------------------------------------------------------

    fn usage_due(&self, state: &State, account: &Account, live: Option<u32>, force: bool, now: i64) -> bool {
        if account.kind != Kind::Oauth || account.needs_login {
            return false;
        }
        let Some(record) = state.usage.get(&account.id) else {
            return true;
        };
        if now < record.retry_after_ms {
            return false;
        }
        if force {
            return true;
        }
        // The account in use moves fastest. The others only move when a
        // session runs on them, so they are read a third as often.
        let ttl = state.settings.usage_ttl_s as i64 * 1000;
        let ttl = if live == Some(account.id) { ttl } else { ttl * 3 };
        now - record.fetched_ms >= ttl
    }

    /// Read usage for every account that is due, in parallel, and return
    /// the full picture. A failed read keeps the last good numbers.
    pub fn snapshot(&self, force: bool, network: bool) -> Result<Snapshot> {
        let live = self.identify_live(network)?;
        let state = self.store.load()?;
        let now = now_ms();
        if network {
            let due: Vec<Account> = state
                .accounts
                .values()
                .filter(|a| self.usage_due(&state, a, live.account, force, now))
                .cloned()
                .collect();
            // Tokens first, one at a time: a refresh takes the operation lock.
            let mut ready: Vec<(Account, Result<Oauth, String>)> = Vec::new();
            for account in due {
                let token = self
                    .ensure_fresh(account.id, &live, false)
                    .map_err(|e| format!("{e:#}"));
                ready.push((account, token));
            }
            let results: Vec<(u32, Result<Fetched, String>, Option<u64>)> = std::thread::scope(|scope| {
                let handles: Vec<_> = ready
                    .iter()
                    .map(|(account, token)| {
                        scope.spawn(move || match token {
                            Err(e) => (account.id, Err(e.clone()), None),
                            Ok(oauth) => match self.fetch(account, oauth) {
                                Ok(f) => (account.id, Ok(f), None),
                                Err(ApiError::RateLimited { retry_after_s }) => (
                                    account.id,
                                    Err("rate limited".to_string()),
                                    Some(retry_after_s.unwrap_or(0)),
                                ),
                                Err(e) => (account.id, Err(e.to_string()), None),
                            },
                        })
                    })
                    .collect();
                handles.into_iter().filter_map(|h| h.join().ok()).collect()
            });
            if !results.is_empty() {
                self.store.update(|s| {
                    let mut samples = Vec::new();
                    for (id, result, rate_limited) in results {
                        let record = s.usage.entry(id).or_insert_with(UsageRecord::default);
                        match result {
                            Ok(fetched) => {
                                let at = now_ms();
                                let sample = Sample::of(id, &fetched.usage, at);
                                let moved = record
                                    .usage
                                    .as_ref()
                                    .is_none_or(|before| sample.moved_since(&Sample::of(id, before, at)));
                                if moved || at - record.sampled_ms >= history::SAMPLE_EVERY_MS {
                                    record.sampled_ms = at;
                                    samples.push(sample);
                                }
                                record.usage = Some(fetched.usage);
                                record.fetched_ms = now_ms();
                                record.error = None;
                                record.retry_after_ms = 0;
                                if let (Some(account), Some(profile)) = (s.accounts.get_mut(&id), fetched.profile) {
                                    account.plan = profile.plan.or(account.plan.take());
                                    account.plan_checked_ms = now_ms();
                                    if let Some(name) = profile.org_name {
                                        account.org_name = name;
                                    }
                                }
                            }
                            Err(message) => {
                                record.error = Some(message);
                                let pause = match rate_limited {
                                    Some(server) => RATE_LIMIT_PAUSE_MS.max(server as i64 * 1000),
                                    None => FAILURE_PAUSE_MS,
                                };
                                record.retry_after_ms = now_ms() + pause;
                            }
                        }
                    }
                    // History is a convenience: failing to write it never
                    // fails the reading.
                    let _ = history::append(&self.store.root, &samples);
                    Ok(())
                })?;
            }
        }
        let mut state = self.store.load()?;
        let working_model = self.working_model();
        let model_setting = state.settings.model.clone();
        state.settings.model = state
            .settings
            .settled_model(working_model.as_deref(), &state.model_names());
        let sessions = self.sessions();
        let now = now_ms();
        let verdicts = rank::evaluate(&state, &sessions, state.settings.strategy, now);
        Ok(Snapshot {
            working_model,
            model_setting,
            live: live.account,
            live_unmanaged: live.oauth.is_some() && live.account.is_none(),
            state,
            sessions,
            verdicts,
            now_ms: now,
        })
    }

    fn fetch(&self, account: &Account, oauth: &Oauth) -> Result<Fetched, ApiError> {
        let usage = self.api.usage(oauth.access())?;
        // The plan changes on a billing cycle, so it is read a few times a
        // day. A failure here never fails the usage reading.
        let profile = (now_ms() - account.plan_checked_ms > PLAN_TTL_MS)
            .then(|| self.api.profile(oauth.access()).ok())
            .flatten()
            .filter(|p| account.uuid.is_empty() || p.identity.uuid == account.uuid);
        Ok(Fetched { usage, profile })
    }

    /// Past usage readings, for trends and forecasts.
    pub fn history(&self) -> History {
        History::load(&self.store.root)
    }

    // ---- choosing ------------------------------------------------------

    /// The best account to start work on now.
    pub fn pick(&self, strategy: Option<Strategy>, snapshot: &Snapshot) -> Option<Verdict> {
        let strategy = strategy.unwrap_or(snapshot.state.settings.strategy);
        rank::evaluate(&snapshot.state, &snapshot.sessions, strategy, snapshot.now_ms)
            .into_iter()
            .find(|v| v.eligible && v.room.as_ref().is_none_or(|r| !r.at_limit))
    }

    /// One pass of automatic switching.
    pub fn auto_step(&self, dry_run: bool) -> Result<(Snapshot, Decision, Option<Switched>)> {
        let snapshot = self.snapshot(false, true)?;
        let decision = rank::decide(
            snapshot.live,
            &snapshot.verdicts,
            &snapshot.state.settings,
            snapshot.now_ms,
            snapshot.state.last_auto_switch_ms,
        );
        let switched = match (&decision, dry_run) {
            (Decision::Switch { to, .. }, false) => {
                let outcome = self.switch_to(*to)?;
                self.store.update(|s| {
                    s.last_auto_switch_ms = now_ms();
                    Ok(())
                })?;
                Some(outcome)
            }
            _ => None,
        };
        Ok((snapshot, decision, switched))
    }

    // ---- running sessions ----------------------------------------------

    fn run_dir(&self) -> std::path::PathBuf {
        self.store.root.join("run")
    }

    /// Record that this process runs Claude Code as `account`, by writing
    /// `<pid>.json` into cx's run directory. [`Engine::run_files`] removes
    /// the file once the process is gone.
    pub fn record_session(&self, account: u32) -> Result<()> {
        let dir = self.run_dir();
        util::mkdir_private(&dir)?;
        let pid = std::process::id();
        util::write_json(
            &dir.join(format!("{pid}.json")),
            &json!({"pid": pid, "account": account, "started_ms": now_ms()}),
            0o600,
        )
    }

    /// `cx run` sessions still alive, with dead ones cleared away.
    pub fn run_files(&self) -> Vec<RunFile> {
        let mut alive = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.run_dir()) else {
            return alive;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let parsed = util::read_json(&path).ok().flatten().and_then(|v| {
                Some(RunFile {
                    pid: v.get("pid").and_then(Value::as_u64)? as u32,
                    account: v.get("account").and_then(Value::as_u64)? as u32,
                })
            });
            match parsed {
                Some(run) if util::pid_alive(run.pid) => alive.push(run),
                _ => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        alive
    }

    /// How many `cx run` sessions are alive, by account number. Accounts
    /// with none are absent.
    pub fn sessions(&self) -> BTreeMap<u32, usize> {
        let mut counts = BTreeMap::new();
        for run in self.run_files() {
            *counts.entry(run.account).or_insert(0) += 1;
        }
        counts
    }

    // ---- adding accounts -----------------------------------------------

    /// Add a login, or replace the stored login of the account it belongs to.
    /// Returns the account number and whether it is new.
    pub fn adopt(&self, oauth: &Oauth, oauth_account: Option<Value>, known: Option<Identity>) -> Result<(u32, bool)> {
        // Ask the server whose login this is. The config's `oauthAccount` is
        // only trusted when the server cannot be reached.
        let profile = self.api.profile(oauth.access()).ok();
        let hinted = oauth_account.as_ref().and_then(claude::identity_of);
        let (uuid, org_uuid) = match (&profile, &known, &hinted) {
            (Some(p), _, _) => (p.identity.uuid.clone(), p.identity.org_uuid.clone().unwrap_or_default()),
            (None, Some(k), _) => (k.uuid.clone(), k.org_uuid.clone().unwrap_or_default()),
            (None, None, Some((uuid, org))) => (uuid.clone(), org.clone()),
            (None, None, None) => {
                bail!("could not tell which account this login belongs to (no reply from the profile endpoint)")
            }
        };
        // An `oauthAccount` that names someone else must not be stored
        // against this login.
        let oauth_account = oauth_account.filter(|v| claude::identity_of(v).is_some_and(|(u, _)| u == uuid));
        let email = profile
            .as_ref()
            .and_then(|p| p.identity.email.clone())
            .or_else(|| known.as_ref().and_then(|k| k.email.clone()))
            .or_else(|| {
                oauth_account
                    .as_ref()
                    .and_then(|v| v.get("emailAddress"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .unwrap_or_else(|| format!("account-{}", uuid.chars().take(8).collect::<String>()));

        let _operation = self.store.lock_operations()?;
        let (id, created) = self.store.update(|state| {
            let existing = state.find_identity(&uuid, &org_uuid);
            let id = existing.unwrap_or_else(|| state.next_id());
            let account = state
                .accounts
                .entry(id)
                .or_insert_with(|| Account::new(id, Kind::Oauth, &email));
            account.label = email.clone();
            account.uuid = uuid.clone();
            if !org_uuid.is_empty() {
                account.org_uuid = org_uuid.clone();
            }
            account.needs_login = false;
            if let Some(p) = &profile {
                account.plan = p.plan.clone().or(account.plan.take());
                account.plan_checked_ms = now_ms();
                if let Some(name) = &p.org_name {
                    account.org_name = name.clone();
                }
            }
            if oauth_account.is_some() {
                account.oauth_account = oauth_account.clone();
            }
            Ok((id, existing.is_none()))
        })?;
        self.write_secret(id, oauth)?;
        Ok((id, created))
    }

    /// Forget an account: its state, usage readings and directory pins, its
    /// secret and the `.prev` copy, and its `cx run` profile directory.
    /// Holds the operation lock. Fails when there is no such account. The
    /// default Claude Code home is left alone, and nothing is revoked on the
    /// server. Returns the account as it was.
    pub fn remove(&self, id: u32) -> Result<Account> {
        let _operation = self.store.lock_operations()?;
        let account = self.store.update(|state| {
            let account = state
                .accounts
                .remove(&id)
                .with_context(|| format!("no account #{id}"))?;
            state.usage.remove(&id);
            state.mappings.retain(|_, owner| *owner != id);
            state.fingerprints.retain(|_, owner| *owner != id);
            Ok(account)
        })?;
        secrets::delete(&account.secret_name())?;
        let _ = secrets::delete(&format!("{}.prev", account.secret_name()));
        let profile = ClaudeHome::profile(id);
        if profile.exists() {
            let _ = profile.delete_credentials();
            let _ = std::fs::remove_dir_all(&profile.dir);
        }
        Ok(account)
    }
}

struct Fetched {
    usage: crate::store::Usage,
    profile: Option<crate::api::Profile>,
}
