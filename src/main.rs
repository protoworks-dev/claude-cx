//! The `cx` command: argument parsing and the one-shot commands. Everything
//! else lives in the `claude_cx` library.

#[cfg(target_os = "macos")]
use claude_cx::menubar;
use claude_cx::{
    accounts, activity, claude, engine, notify, rank, run, secrets, service, store, transfer, tui, ui, util,
};

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use engine::Engine;
use rank::Decision;
use serde_json::{Value, json};
use std::io::{IsTerminal, Read, Write};
use store::{Kind, Strategy};

#[derive(Parser)]
#[command(
    name = "cx",
    version,
    about = "Several Claude accounts behind one Claude Code",
    long_about = "cx keeps a login for each of your Claude accounts, shows how much of each \
one's limits is used, and puts your work on the account with the most room.\n\n\
Run `cx` alone for the live dashboard.",
    after_help = "Start here:\n  cx import-cswap     bring your accounts over from claude-swap\n  cx login            sign in to another account in the browser\n  cx                  open the dashboard\n  cx run              start Claude Code on the account with the most room\n  cx auto             keep the default login on an account with room"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List accounts with their 5-hour and weekly usage
    #[command(visible_alias = "list")]
    Ls {
        /// Read usage from the server now, even if the last reading is recent
        #[arg(long)]
        fresh: bool,
        /// Show the last readings without any network call
        #[arg(long)]
        offline: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Make an account the default Claude Code login (no account: the best one)
    #[command(visible_alias = "switch")]
    Use {
        /// Number, alias or email. Omit to let cx choose.
        account: Option<String>,
        #[arg(long)]
        strategy: Option<Strategy>,
    },
    /// Start Claude Code on an account in this terminal only
    Run {
        /// Number, alias or email. Omit to let cx choose.
        account: Option<String>,
        #[arg(long)]
        strategy: Option<Strategy>,
        /// Arguments for claude, after `--`
        #[arg(last = true)]
        claude_args: Vec<String>,
    },
    /// Watch usage and move the default login before it hits a limit
    Auto {
        /// Check once and exit: 0 switched, 2 nothing to do, 1 error
        #[arg(long)]
        once: bool,
        /// Say what would happen without switching
        #[arg(long)]
        dry_run: bool,
        /// Seconds between checks
        #[arg(long)]
        interval: Option<u64>,
    },
    /// Explain how cx ranks the accounts right now
    Why {
        #[arg(long)]
        strategy: Option<Strategy>,
    },
    /// Sign in to an account in the browser (new, or one that needs it again)
    Login {
        /// An existing account to sign in to again
        account: Option<String>,
        /// Fill in this email on the sign-in page
        #[arg(long)]
        email: Option<String>,
        /// Sign in with SSO
        #[arg(long)]
        sso: bool,
    },
    /// Take over the login Claude Code is using now
    Add,
    /// Add a Console API key or a setup-token, read from stdin
    AddKey {
        /// A name for it
        #[arg(long)]
        label: Option<String>,
    },
    /// Bring accounts over from claude-swap
    ImportCswap {
        /// Show what would be imported and change nothing
        #[arg(long)]
        dry_run: bool,
        /// claude-swap's data directory, when it is not in the usual place
        #[arg(long)]
        from: Option<std::path::PathBuf>,
    },
    /// Refresh an account's token now (no account: every account)
    Refresh { account: Option<String> },
    /// Remove an account and its stored login
    Rm {
        account: String,
        /// Do not ask
        #[arg(long, short)]
        yes: bool,
    },
    /// Give an account a short name (no name: remove it)
    Alias { account: String, name: Option<String> },
    /// Return an account to automatic selection
    Enable { account: String },
    /// Hold an account out of automatic selection
    Disable { account: String },
    /// Prefer an account: a higher number is used first (0 is the default)
    Priority { account: String, level: i32 },
    /// Show settings, or set one
    Config { key: Option<String>, value: Option<String> },
    /// Run `cx auto` in the background at login (macOS)
    Service {
        #[command(subcommand)]
        action: ServiceCmd,
    },
    /// Pin a directory to an account, so `cx run` there starts on it (no arguments: list the pins)
    Map {
        /// Number, alias or email
        account: Option<String>,
        /// The directory to pin; this one when omitted
        dir: Option<std::path::PathBuf>,
    },
    /// Remove a directory's pin
    Unmap {
        /// The directory; this one when omitted
        dir: Option<std::path::PathBuf>,
    },
    /// Write every account, login and setting to one passphrase-encrypted file
    Export {
        file: std::path::PathBuf,
        /// Replace the file if it exists
        #[arg(long)]
        force: bool,
    },
    /// Bring accounts in from a `cx export` file
    Import {
        file: std::path::PathBuf,
        /// Also take the file's settings and directory pins
        #[arg(long)]
        settings: bool,
    },
    /// One line for Claude Code's status bar: the account in use and its limits
    Statusline,
    /// Print a shell completion script (bash, zsh, fish, elvish, powershell)
    Completions { shell: clap_complete::Shell },
    /// Show the accounts in the macOS menu bar
    Menubar {
        #[command(subcommand)]
        action: Option<ServiceCmd>,
    },
    /// Install the cx plugin for Claude Code, or bring it up to date
    Plugin {
        #[command(subcommand)]
        action: Option<PluginCmd>,
    },
    /// Check the setup and report anything wrong
    Doctor,
    /// Print usage and waste as JSON, for the Claude Code plugin and other tools
    Report {
        /// How many days back to look (1 is today)
        #[arg(long, default_value_t = 7)]
        days: i64,
        /// Show the stops and corrections in one transcript, with the text around them
        #[arg(long, value_name = "TRANSCRIPT")]
        moments: Option<std::path::PathBuf>,
        /// How many moments to show
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Record a Claude Code hook event read from standard input (used by the plugin)
    Hook,
    /// Read the local transcripts and print each account's totals
    #[command(hide = true)]
    Activity,
    /// Print the usage endpoint's reply for an account, as the server sent it
    #[command(hide = true)]
    RawUsage { account: String },
}

#[derive(Subcommand)]
enum PluginCmd {
    /// Add the Protoworks marketplace and install the plugin, or update it (the default)
    Install,
    /// Say whether Claude Code has the plugin, and which version
    Status,
    /// Uninstall the plugin from Claude Code
    Remove,
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Start it now and at every login
    Install,
    /// Stop it and remove it from login
    Uninstall,
    /// Say whether it is installed and running
    Status,
}

fn main() {
    let cli = Cli::parse();
    let code = match dispatch(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{} {error:#}", ui::red("error:"));
            1
        }
    };
    std::process::exit(code);
}

fn dispatch(cli: Cli) -> Result<i32> {
    // A hook runs inside someone's session: it does the least it can and
    // never fails.
    if matches!(cli.command, Some(Cmd::Hook)) {
        claude_cx::hook::run(&claude_cx::util::store_root());
        return Ok(0);
    }
    if let Some(Cmd::Report {
        moments: Some(transcript),
        limit,
        ..
    }) = &cli.command
    {
        println!(
            "{}",
            serde_json::to_string_pretty(&claude_cx::report::moments(transcript, *limit))?
        );
        return Ok(0);
    }
    let engine = Engine::open()?;
    // Logins an earlier version left unencrypted are encrypted on the way
    // in. A failure here is reported by `cx doctor`, not by every command.
    let _ = secrets::encrypt_leftovers();
    match cli.command {
        None => {
            if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
                tui::run(engine)?;
            } else {
                print!("{}", ui::render_list(&engine.snapshot(false, true)?));
            }
        }
        Some(Cmd::Ls { fresh, offline, json }) => {
            let snapshot = engine.snapshot(fresh, !offline)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot_json(&snapshot))?);
            } else {
                print!("{}", ui::render_list(&snapshot));
            }
        }
        Some(Cmd::Use { account, strategy }) => {
            let id = match account {
                Some(query) => engine.store.load()?.resolve(&query)?.id,
                None => {
                    let snapshot = engine.snapshot(false, true)?;
                    let best = engine
                        .pick(strategy, &snapshot)
                        .context("no account has room right now (see `cx ls`)")?;
                    best.id
                }
            };
            let outcome = engine.switch_to(id)?;
            report_switch(&engine, &outcome)?;
        }
        Some(Cmd::Run {
            account,
            strategy,
            claude_args,
        }) => {
            let launch = match account {
                Some(query) => run::Launch {
                    account: engine.store.load()?.resolve(&query)?.clone(),
                    why: "chosen by you".into(),
                },
                None => match pinned(&engine)? {
                    Some(launch) => launch,
                    None => run::choose(&engine, strategy)?,
                },
            };
            run::exec(&engine, &launch, &claude_args)?;
        }
        Some(Cmd::Auto {
            once,
            dry_run,
            interval,
        }) => return auto(&engine, once, dry_run, interval),
        Some(Cmd::Why { strategy }) => why(&engine, strategy)?,
        Some(Cmd::Login { account, email, sso }) => {
            let state = engine.store.load()?;
            let target = account.as_deref().map(|q| state.resolve(q)).transpose()?;
            let email = email.or_else(|| target.filter(|a| a.kind == Kind::Oauth).map(|a| a.label.clone()));
            ui::note("Opening the sign-in page. Use the browser profile that belongs to this account.");
            let (id, created) = accounts::login(&engine, email.as_deref(), sso)?;
            let state = engine.store.load()?;
            let signed_in = &state.accounts[&id];
            if let Some(wanted) = target.filter(|a| a.id != id) {
                ui::warn(&format!(
                    "you signed in as {}, not {}. {} still needs its own sign-in.",
                    signed_in.label,
                    wanted.label,
                    wanted.display()
                ));
            }
            ui::ok(&format!(
                "{} {}",
                if created { "added" } else { "signed in again to" },
                signed_in.display()
            ));
        }
        Some(Cmd::Add) => {
            let (id, created) = accounts::add_current(&engine)?;
            let state = engine.store.load()?;
            ui::ok(&format!(
                "{} {}",
                if created { "added" } else { "updated the login of" },
                state.accounts[&id].display()
            ));
        }
        Some(Cmd::AddKey { label }) => {
            let key = read_secret("Paste the key (it will not be shown): ")?;
            let account = accounts::add_key(&engine, &key, label.as_deref())?;
            ui::ok(&format!(
                "added {}. Start Claude Code with it: cx run {}",
                account.display(),
                account.id
            ));
        }
        Some(Cmd::ImportCswap { dry_run, from }) => {
            let root = from.unwrap_or_else(accounts::cswap_root);
            let rows = accounts::import_cswap(&engine, &root, dry_run)?;
            for row in &rows {
                let mark = if row.ok { ui::green("✓") } else { ui::red("✗") };
                println!("{mark} slot {:>2}  {}  {}", row.slot, row.label, ui::dim(&row.outcome));
            }
            if dry_run {
                println!("\nNothing was changed. Run `cx import-cswap` to import.");
            } else {
                println!(
                    "\n{}",
                    ui::yellow(
                        "Stop using cswap from here on: both tools now hold the same refresh tokens, and the first refresh by one retires the other's copy."
                    )
                );
            }
        }
        Some(Cmd::Refresh { account }) => {
            let state = engine.store.load()?;
            let ids: Vec<u32> = match account {
                Some(query) => vec![state.resolve(&query)?.id],
                None => state
                    .accounts
                    .values()
                    .filter(|a| a.kind == Kind::Oauth)
                    .map(|a| a.id)
                    .collect(),
            };
            let live = engine.identify_live(true)?;
            let mut failed = false;
            for id in ids {
                let name = state.accounts[&id].display();
                match engine.ensure_fresh(id, &live, true) {
                    Ok(oauth) => ui::ok(&format!(
                        "{name}: token valid for {}",
                        util::human_duration((oauth.expires_ms() - util::now_ms()) / 1000)
                    )),
                    Err(e) => {
                        failed = true;
                        ui::warn(&format!("{e:#}"));
                    }
                }
            }
            return Ok(failed as i32);
        }
        Some(Cmd::Rm { account, yes }) => {
            let target = engine.store.load()?.resolve(&account)?.clone();
            if !yes && !confirm(&format!("Remove {} and delete its stored login?", target.display()))? {
                println!("Nothing was removed.");
                return Ok(0);
            }
            engine.remove(target.id)?;
            ui::ok(&format!("removed {}", target.display()));
        }
        Some(Cmd::Alias { account, name }) => {
            let display = engine.store.update(|state| {
                let id = state.resolve(&account)?.id;
                state.set_alias(id, name.as_deref())
            })?;
            ui::ok(&display);
        }
        Some(Cmd::Enable { account }) => set_flag(&engine, &account, false)?,
        Some(Cmd::Disable { account }) => set_flag(&engine, &account, true)?,
        Some(Cmd::Priority { account, level }) => {
            let display = engine.store.update(|state| {
                let id = state.resolve(&account)?.id;
                let a = state.accounts.get_mut(&id).unwrap();
                a.priority = level;
                Ok(a.display())
            })?;
            ui::ok(&format!("{display}: priority {level}"));
        }
        Some(Cmd::Config { key, value }) => config(&engine, key, value)?,
        Some(Cmd::Service { action }) => match action {
            ServiceCmd::Install => service::install(&service::AUTO)?,
            ServiceCmd::Uninstall => service::uninstall(&service::AUTO)?,
            ServiceCmd::Status => service::status(&service::AUTO)?,
        },
        #[cfg(target_os = "macos")]
        Some(Cmd::Menubar { action }) => match action {
            None => menubar::run()?,
            Some(ServiceCmd::Install) => service::install(&menubar::UNIT)?,
            Some(ServiceCmd::Uninstall) => service::uninstall(&menubar::UNIT)?,
            Some(ServiceCmd::Status) => service::status(&menubar::UNIT)?,
        },
        #[cfg(not(target_os = "macos"))]
        Some(Cmd::Menubar { .. }) => bail!("the menu-bar item is macOS only"),
        Some(Cmd::Map { account, dir }) => map(&engine, account, dir)?,
        Some(Cmd::Unmap { dir }) => {
            let dir = real_dir(dir)?;
            let removed = engine.store.update(|state| Ok(state.mappings.remove(&dir).is_some()))?;
            if removed {
                ui::ok(&format!("{dir} is no longer pinned"));
            } else {
                println!("{dir} was not pinned. `cx map` lists the pins.");
            }
        }
        Some(Cmd::Export { file, force }) => {
            if file.exists() && !force {
                bail!("{} exists. Pass --force to replace it.", file.display());
            }
            let passphrase = new_passphrase()?;
            let (sealed, count) = transfer::export(&engine, &passphrase)?;
            util::atomic_write(&file, &sealed, 0o600)?;
            ui::ok(&format!("wrote {count} account(s) to {}", file.display()));
            ui::note(
                "The file holds every login. On a second machine, the first refresh there retires this machine's copy of that login, and the other way round.",
            );
        }
        Some(Cmd::Import { file, settings }) => {
            let sealed = std::fs::read(&file).with_context(|| format!("read {}", file.display()))?;
            let passphrase = match std::env::var("CX_PASSPHRASE") {
                Ok(p) => p,
                Err(_) => read_secret("Passphrase: ")?,
            };
            for row in transfer::import(&engine, &sealed, &passphrase, settings)? {
                println!("{} {}  {}", ui::green("✓"), row.label, ui::dim(&row.outcome));
            }
        }
        Some(Cmd::Statusline) => println!("{}", statusline(&engine)?),
        Some(Cmd::Completions { shell }) => {
            clap_complete::generate(shell, &mut Cli::command(), "cx", &mut std::io::stdout());
        }
        Some(Cmd::Plugin { action }) => match action.unwrap_or(PluginCmd::Install) {
            PluginCmd::Install => {
                ui::note("Asking Claude Code to install the plugin from github.com/protoworks-dev/claude-plugins…");
                for line in claude_cx::plugin::install()? {
                    ui::ok(&line);
                }
                ui::note("Claude Code sessions that are open now load it when they are restarted.");
            }
            PluginCmd::Status => ui::note(&plugin_status(&claude_cx::plugin::status()?)),
            PluginCmd::Remove => {
                if claude_cx::plugin::remove()? {
                    ui::ok("the cx plugin was removed from Claude Code");
                } else {
                    ui::note("the cx plugin is not installed");
                }
            }
        },
        Some(Cmd::Doctor) => return doctor(&engine),
        Some(Cmd::Hook) => {}
        Some(Cmd::Report { days, .. }) => {
            let mut activity = activity::Activity::load(&engine.store.root);
            activity.refresh(&claude::ClaudeHome::default_home().dir, &engine.store.root);
            let state = engine.store.load()?;
            let live = engine.identify_live(false).ok().and_then(|live| live.account);
            let report = claude_cx::report::build(
                &state,
                live,
                &activity,
                &engine.store.root,
                days,
                claude_cx::util::now_ms(),
            );
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Some(Cmd::Activity) => {
            let started = std::time::Instant::now();
            let mut activity = activity::Activity::load(&engine.store.root);
            let changed = activity.refresh(&claude::ClaudeHome::default_home().dir, &engine.store.root);
            println!("read in {:.1}s (changed: {changed})", started.elapsed().as_secs_f64());
            let state = engine.store.load()?;
            let since = (chrono::Local::now() - chrono::Duration::days(activity::DAYS - 1))
                .format("%Y-%m-%d")
                .to_string();
            for account in state.accounts.values().filter(|a| !a.uuid.is_empty()) {
                let s = activity.summary(&account.uuid, &since);
                println!(
                    "#{} requests {} output {} cache-write {} cache-read {} rebuilds {} subagent-requests {} models {:?} hooks {:?}",
                    account.id,
                    s.total.requests,
                    s.total.output,
                    s.total.cache_write,
                    s.total.cache_read,
                    s.total.rebuilds,
                    s.subagents.requests,
                    s.models
                        .iter()
                        .map(|(m, t)| (m.as_str(), t.requests))
                        .collect::<Vec<_>>(),
                    s.hooks
                        .iter()
                        .map(|(e, n, ms)| (e.as_str(), *n, ms / 1000))
                        .collect::<Vec<_>>(),
                );
            }
            println!("latest model: {:?}", activity.latest_model().map(|m| m.0));
        }
        Some(Cmd::RawUsage { account }) => {
            let id = engine.store.load()?.resolve(&account)?.id;
            let live = engine.identify_live(true)?;
            let oauth = engine.ensure_fresh(id, &live, false)?;
            let raw = engine
                .api
                .usage_raw(oauth.access())
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("{}", serde_json::to_string_pretty(&raw)?);
        }
    }
    Ok(0)
}

fn report_switch(engine: &Engine, outcome: &engine::Switched) -> Result<()> {
    let state = engine.store.load()?;
    let name = state.accounts[&outcome.to].display();
    if !outcome.changed {
        ui::ok(&format!("{name} is already the default login"));
        return Ok(());
    }
    ui::ok(&format!("default login is now {name}"));
    if let Some(stash) = &outcome.stashed {
        ui::warn(&format!(
            "the previous default login was not one of cx's accounts. It was kept as '{stash}' in {}.",
            secrets::place()
        ));
    }
    ui::note("Running Claude Code sessions pick it up within about 30 seconds; a restart applies it at once.");
    Ok(())
}

fn set_flag(engine: &Engine, account: &str, disabled: bool) -> Result<()> {
    let display = engine.store.update(|state| {
        let id = state.resolve(account)?.id;
        let a = state.accounts.get_mut(&id).unwrap();
        a.disabled = disabled;
        Ok(a.display())
    })?;
    ui::ok(&format!(
        "{display} is {}",
        if disabled {
            "held out of automatic selection"
        } else {
            "back in automatic selection"
        }
    ));
    Ok(())
}

fn why(engine: &Engine, strategy: Option<Strategy>) -> Result<()> {
    let snapshot = engine.snapshot(false, true)?;
    let strategy = strategy.unwrap_or(snapshot.state.settings.strategy);
    let verdicts = rank::evaluate(&snapshot.state, &snapshot.sessions, strategy, snapshot.now_ms);
    println!("Strategy {}. Best first.\n", ui::bold(&strategy.to_string()));
    for (rank, verdict) in verdicts.iter().enumerate() {
        let account = &snapshot.state.accounts[&verdict.id];
        let active = if snapshot.live == Some(verdict.id) {
            ui::green(" ●")
        } else {
            "  ".into()
        };
        if verdict.eligible {
            println!(
                "{:>2}.{active} {}  {}",
                rank + 1,
                ui::bold(&account.display()),
                ui::dim(&verdict.note)
            );
        } else {
            println!(
                "  -{active} {}  {}",
                ui::dim(&account.display()),
                ui::yellow(&verdict.note)
            );
        }
    }
    let decision = rank::decide(
        snapshot.live,
        &verdicts,
        &snapshot.state.settings,
        snapshot.now_ms,
        snapshot.state.last_auto_switch_ms,
    );
    println!();
    match decision {
        Decision::Stay(reason) => println!("`cx auto` would stay: {reason}."),
        Decision::Switch { to, reason } => {
            println!(
                "`cx auto` would switch to {}: {reason}.",
                snapshot.state.accounts[&to].display()
            )
        }
    }
    Ok(())
}

fn auto(engine: &Engine, once: bool, dry_run: bool, interval: Option<u64>) -> Result<i32> {
    let mut last_line = String::new();
    let mut watch = notify::Watch::default();
    loop {
        let stamp = chrono::Local::now().format("%H:%M:%S");
        let mut switched = false;
        match engine.auto_step(dry_run) {
            Ok((snapshot, decision, outcome)) => {
                let name = |id: u32| {
                    snapshot
                        .state
                        .accounts
                        .get(&id)
                        .map(|a| a.display())
                        .unwrap_or_else(|| format!("#{id}"))
                };
                let line = match (&decision, &outcome) {
                    (Decision::Switch { to, reason }, Some(_)) => {
                        switched = true;
                        format!("switched to {} ({reason})", name(*to))
                    }
                    (Decision::Switch { to, reason }, None) => format!("would switch to {} ({reason})", name(*to)),
                    (Decision::Stay(reason), _) => match snapshot.live {
                        Some(id) => format!("staying on {} ({reason})", name(id)),
                        None => format!("holding ({reason})"),
                    },
                };
                if !dry_run && !once {
                    let moved = match (&decision, &outcome) {
                        (Decision::Switch { to, reason }, Some(_)) => Some((*to, reason.as_str())),
                        _ => None,
                    };
                    watch.observe(&snapshot, moved);
                }
                // A line is printed when something changed, not on every check.
                if line != last_line || once {
                    println!("{} {line}", ui::dim(&stamp.to_string()));
                    std::io::stdout().flush().ok();
                    last_line = line;
                }
            }
            Err(e) => {
                eprintln!("{} {} {e:#}", ui::dim(&stamp.to_string()), ui::red("error:"));
                if once {
                    return Ok(1);
                }
            }
        }
        if once {
            return Ok(if switched { 0 } else { 2 });
        }
        let seconds = interval
            .unwrap_or(engine.store.load().map(|s| s.settings.interval_s).unwrap_or(60))
            .max(15);
        std::thread::sleep(std::time::Duration::from_secs(seconds));
    }
}

fn config(engine: &Engine, key: Option<String>, value: Option<String>) -> Result<()> {
    let settings = engine.store.load()?.settings;
    let Some(key) = key else {
        for (name, help) in store::SETTING_HELP {
            let mark = if settings.is_default(name) { " (default)" } else { "" };
            let shown = format!("{}{mark}", settings.show(name).unwrap_or_default());
            println!("{name:<14} {shown}");
            println!("{:<14} {}", "", ui::dim(help));
        }
        println!("\n{}", ui::dim("Set one with: cx config <key> <value>"));
        return Ok(());
    };
    let Some(existing) = settings.show(&key) else {
        bail!("unknown setting '{key}'. `cx config` lists them.");
    };
    let Some(value) = value else {
        println!("{existing}");
        return Ok(());
    };
    engine.store.update(|state| state.settings.set(&key, &value))?;
    ui::ok(&format!("{key} = {value}"));
    Ok(())
}

/// The account pinned to the current directory, if any.
fn pinned(engine: &Engine) -> Result<Option<run::Launch>> {
    let Ok(here) = std::env::current_dir().and_then(|d| d.canonicalize()) else {
        return Ok(None);
    };
    let state = engine.store.load()?;
    Ok(state.mapped(&here).map(|(dir, account)| run::Launch {
        account: account.clone(),
        why: format!("{dir} is pinned to it"),
    }))
}

fn real_dir(dir: Option<std::path::PathBuf>) -> Result<String> {
    let dir = match dir {
        Some(d) => d,
        None => std::env::current_dir()?,
    };
    let real = dir
        .canonicalize()
        .with_context(|| format!("{} is not a directory here", dir.display()))?;
    real.to_str()
        .map(String::from)
        .context("the directory's path is not valid UTF-8")
}

fn map(engine: &Engine, account: Option<String>, dir: Option<std::path::PathBuf>) -> Result<()> {
    let Some(account) = account else {
        let state = engine.store.load()?;
        if state.mappings.is_empty() {
            println!("No directory is pinned. `cx map <account>` pins this one.");
        }
        for (dir, id) in &state.mappings {
            let name = state
                .accounts
                .get(id)
                .map(|a| a.display())
                .unwrap_or_else(|| format!("#{id}"));
            println!("{dir}  {}  {}", ui::dim("→"), ui::bold(&name));
        }
        return Ok(());
    };
    let dir = real_dir(dir)?;
    let name = engine.store.update(|state| {
        let target = state.resolve(&account)?;
        let (id, name) = (target.id, target.display());
        state.mappings.insert(dir.clone(), id);
        Ok(name)
    })?;
    ui::ok(&format!("`cx run` in {dir} and below now starts on {name}"));
    Ok(())
}

/// A passphrase for a new export: from `CX_PASSPHRASE`, or typed twice.
fn new_passphrase() -> Result<String> {
    if let Ok(passphrase) = std::env::var("CX_PASSPHRASE") {
        return Ok(passphrase);
    }
    let first = read_secret("Passphrase for the file: ")?;
    if std::io::stdin().is_terminal() && read_secret("Again: ")? != first {
        bail!("the two passphrases differ");
    }
    Ok(first)
}

/// One line about the account this Claude Code session runs on. Reads only
/// what cx already knows: no network call, so it is safe to run on every
/// status bar update.
fn statusline(engine: &Engine) -> Result<String> {
    let state = engine.store.load()?;
    let id = match std::env::var("CX_ACCOUNT").ok().and_then(|v| v.parse::<u32>().ok()) {
        Some(id) => Some(id),
        None => engine.identify_live(false)?.account,
    };
    let Some(account) = id.and_then(|id| state.accounts.get(&id)) else {
        return Ok("cx: login not managed".into());
    };
    let color = std::env::var_os("NO_COLOR").is_none();
    let paint = |text: String, level: ui::Level| {
        let code = match level {
            ui::Level::Calm => "32",
            ui::Level::Busy => "33",
            ui::Level::Over => "31",
        };
        if color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text
        }
    };
    let now = util::now_ms();
    let mut parts = vec![match &account.alias {
        Some(alias) => format!("#{} {alias}", account.id),
        None => format!(
            "#{} {}",
            account.id,
            account.label.split('@').next().unwrap_or(&account.label)
        ),
    }];
    if account.needs_login {
        parts.push(paint("needs sign-in".into(), ui::Level::Over));
    }
    if let Some(usage) = state.usage.get(&account.id).and_then(|r| r.usage.as_ref()) {
        let settings = &state.settings;
        let mut windows = vec![
            ("5h".to_string(), usage.five_hour.as_ref(), settings.threshold_5h),
            ("wk".to_string(), usage.seven_day.as_ref(), settings.threshold_7d),
        ];
        for model in &usage.models {
            windows.push((model.name.clone(), Some(&model.window), settings.threshold_7d));
        }
        for (label, window, line) in windows {
            if let Some(pct) = window.and_then(|w| w.pct_at(now)) {
                parts.push(paint(format!("{label} {pct:.0}%"), ui::level(pct, line)));
            }
        }
    }
    Ok(parts.join(" · "))
}

fn plugin_status(status: &claude_cx::plugin::Status) -> String {
    match (&status.version, status.enabled) {
        (None, _) => "the cx plugin for Claude Code is not installed. `cx plugin` installs it.".to_string(),
        (Some(version), true) => format!("the cx plugin for Claude Code is installed ({version})"),
        (Some(version), false) => {
            format!(
                "the cx plugin for Claude Code is installed ({version}) but switched off. `cx plugin` switches it on."
            )
        }
    }
}

fn doctor(engine: &Engine) -> Result<i32> {
    let mut problems = 0;
    let mut check = |good: bool, text: String| {
        if good {
            println!("{} {text}", ui::green("✓"));
        } else {
            problems += 1;
            println!("{} {text}", ui::red("✗"));
        }
    };
    let version = util::run_bounded(
        std::process::Command::new(run::claude_binary()).arg("--version"),
        None,
        std::time::Duration::from_secs(20),
    );
    match version {
        Ok(out) if out.code == 0 => check(true, format!("Claude Code found: {}", out.stdout.trim())),
        _ => check(
            false,
            format!(
                "`{}` could not be run. Install Claude Code or set CX_CLAUDE_BIN.",
                run::claude_binary()
            ),
        ),
    }
    check(true, format!("secrets are kept in {}", secrets::place()));
    match secrets::backend() {
        secrets::Backend::Age | secrets::Backend::Fnox => {
            let (key, source) = secrets::age_key_file();
            let whose = match source {
                secrets::KeySource::Env(name) => format!("named by {name}"),
                secrets::KeySource::Shared => "shared with fnox, mise or sops".to_string(),
                secrets::KeySource::Own => "cx's own".to_string(),
            };
            // With fnox the key only matters while cx's own config is in use.
            if secrets::backend() == secrets::Backend::Age || !secrets::fnox_config().exists() {
                check(
                    key.is_file() || source == secrets::KeySource::Own,
                    format!(
                        "age key: {} ({whose}{})",
                        key.display(),
                        if key.is_file() {
                            ""
                        } else {
                            ", made when the first login is stored"
                        }
                    ),
                );
            }
            match secrets::encrypt_leftovers() {
                Ok(0) => {}
                Ok(moved) => check(true, format!("{moved} secret(s) from an earlier store were encrypted")),
                Err(error) => check(
                    false,
                    format!("secrets from an earlier store could not be moved: {error:#}"),
                ),
            }
        }
        secrets::Backend::File => check(
            true,
            "CX_SECRETS=file: the logins are readable by you only, but not encrypted".to_string(),
        ),
        secrets::Backend::Keychain => {}
    }
    // The plugin is optional, so its absence is said and not counted.
    if let Ok(status) = claude_cx::plugin::status() {
        println!("  {}", plugin_status(&status));
    }
    for name in claude::AUTH_OVERRIDE_ENV {
        if std::env::var_os(name).is_some() {
            check(
                false,
                format!(
                    "{name} is set in this shell. Claude Code uses it in place of any stored login, so switching has no effect until it is unset."
                ),
            );
        }
    }
    let state = engine.store.load()?;
    check(
        !state.accounts.is_empty(),
        format!("{} account(s) stored", state.accounts.len()),
    );
    let live = engine.identify_live(true)?;
    match (&live.oauth, live.account) {
        (None, _) => check(
            false,
            "Claude Code's default home is not signed in. Run `cx use`.".into(),
        ),
        (Some(_), None) => check(
            false,
            "the default login is not one of cx's accounts. Run `cx add` to take it over.".into(),
        ),
        (Some(_), Some(id)) => check(
            true,
            format!(
                "default login is {}{}",
                state.accounts[&id].display(),
                if live.verified {
                    ""
                } else {
                    " (not confirmed by the server)"
                }
            ),
        ),
    }
    let now = util::now_ms();
    for account in state.accounts.values() {
        let name = account.display();
        match account.kind {
            Kind::Oauth => match engine.read_secret(account) {
                Ok(Some(oauth)) => {
                    let left = (oauth.expires_ms() - now) / 1000;
                    let token = if left > 0 {
                        format!("access token valid for {}", util::human_duration(left))
                    } else {
                        "access token expired, will refresh on use".into()
                    };
                    if account.needs_login {
                        check(
                            false,
                            format!("{name}: refresh token rejected. Run `cx login {}`.", account.id),
                        );
                    } else if oauth.refresh().is_none() {
                        check(
                            false,
                            format!("{name}: no refresh token stored. Run `cx login {}`.", account.id),
                        );
                    } else {
                        check(true, format!("{name}: {token}"));
                    }
                    let profile = claude::ClaudeHome::profile(account.id);
                    if profile.exists() {
                        match profile.read_oauth() {
                            Ok(Some(p)) if p.same_tokens(&oauth) => {}
                            Ok(Some(_)) => check(
                                true,
                                format!(
                                    "{name}: its `cx run` profile holds a different token; the next use brings them in step"
                                ),
                            ),
                            Ok(None) => {}
                            Err(e) => check(false, format!("{name}: could not read its profile login: {e}")),
                        }
                    }
                }
                Ok(None) => check(
                    false,
                    format!("{name}: no login stored. Run `cx login {}`.", account.id),
                ),
                Err(e) => check(false, format!("{name}: could not read its login: {e}")),
            },
            _ => check(
                secrets::get(&account.secret_name()).ok().flatten().is_some(),
                format!("{name}: key stored"),
            ),
        }
    }
    if accounts::cswap_root().join("sequence.json").exists() && !state.accounts.is_empty() {
        println!(
            "{} claude-swap data is still present. Use one tool only: a token refreshed by one is dead in the other.",
            ui::yellow("!")
        );
    }
    println!();
    if problems == 0 {
        println!("Everything checks out.");
    } else {
        println!("{problems} thing(s) need attention.");
    }
    Ok((problems > 0) as i32)
}

fn snapshot_json(snapshot: &engine::Snapshot) -> Value {
    let accounts: Vec<Value> = snapshot
        .state
        .accounts
        .values()
        .map(|a| {
            let record = snapshot.state.usage.get(&a.id);
            let verdict = snapshot.verdict(a.id);
            json!({
                "id": a.id,
                "label": a.label,
                "alias": a.alias,
                "kind": a.kind,
                "plan": a.plan,
                "active": snapshot.live == Some(a.id),
                "disabled": a.disabled,
                "needsLogin": a.needs_login,
                "priority": a.priority,
                "sessions": snapshot.sessions.get(&a.id).copied().unwrap_or(0),
                "usage": record.and_then(|r| r.usage.as_ref()),
                "usageAgeSeconds": record.filter(|r| r.usage.is_some()).map(|r| (snapshot.now_ms - r.fetched_ms) / 1000),
                "usageError": record.and_then(|r| r.error.as_ref()),
                "eligible": verdict.is_some_and(|v| v.eligible),
                "room": verdict.and_then(|v| v.room.as_ref()).map(|r| r.min),
                "note": verdict.map(|v| v.note.clone()),
            })
        })
        .collect();
    json!({
        "schemaVersion": 1,
        "activeAccount": snapshot.live,
        "defaultLoginUnmanaged": snapshot.live_unmanaged,
        "strategy": snapshot.state.settings.strategy,
        "accounts": accounts,
    })
}

fn confirm(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("this needs confirmation; pass --yes to do it without asking");
    }
    eprint!("{question} [y/N] ");
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// Read one line without showing it. From a pipe, read all of stdin.
fn read_secret(prompt: &str) -> Result<String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut text = String::new();
        stdin.lock().read_to_string(&mut text)?;
        return Ok(text.trim().to_string());
    }
    eprint!("{prompt}");
    let fd = 0;
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    let have_term = unsafe { libc::tcgetattr(fd, &mut term) } == 0;
    if have_term {
        let mut silent = term;
        silent.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &silent) };
    }
    let mut line = String::new();
    let read = stdin.read_line(&mut line);
    if have_term {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) };
    }
    eprintln!();
    read?;
    Ok(line.trim().to_string())
}
