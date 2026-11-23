# claude-cx

[![crates.io](https://img.shields.io/crates/v/claude-cx.svg)](https://crates.io/crates/claude-cx)
[![docs.rs](https://docs.rs/claude-cx/badge.svg)](https://docs.rs/claude-cx)
[![CI](https://github.com/protoworks-dev/claude-cx/actions/workflows/ci.yml/badge.svg)](https://github.com/protoworks-dev/claude-cx/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/claude-cx.svg)](LICENSE)

Several Claude accounts behind one Claude Code. The `cx` command keeps a login for each account, shows how much of each one's limits is used and what used it, and puts your work on the account with the most room.

It is a Rust rewrite of the ideas in [claude-swap](https://github.com/realiti4/claude-swap), with its own sign-in, its own token renewal, a load balancer and a dashboard.

> **Not an Anthropic product.** claude-cx is not affiliated with or endorsed by Anthropic. Whether you may hold and rotate several accounts is governed by Anthropic's terms for those accounts; read them before using this, because the accounts are yours to lose. The tool only ever talks to Anthropic's own endpoints with your own logins.

## Install

```bash
cargo install --locked claude-cx
```

This installs one command, `cx`. It needs [Claude Code](https://claude.com/claude-code) installed, and Rust 1.88 or newer and a C compiler to build. `--locked` builds with the dependency versions the release was tested with. To install the code as it is on `main`: `cargo install --locked --git https://github.com/protoworks-dev/claude-cx`.

macOS is the main platform: logins live in the Keychain. On Linux, which has no Keychain, cx encrypts them with [age](https://age-encryption.org) (see [Where logins are kept](#where-logins-are-kept)); the menu-bar item and the background service are macOS only.

If `cargo` itself fails before anything compiles, the cause is the toolchain setup and not cx: `rustc -vV` must work in the directory you install from. Two cases seen: a version manager's `cargo` shim with no version chosen for that directory (use rustup's `~/.cargo/bin/cargo`), and a `RUSTC_WRAPPER` such as sccache whose server is not answering (`RUSTC_WRAPPER= cargo install --locked claude-cx` builds without it).

## Start

```bash
cx import-cswap --dry-run   # see what would come over from claude-swap
cx import-cswap             # bring the accounts over (then stop using cswap)
cx                          # the dashboard
```

Without claude-swap: `cx add` takes over the login Claude Code is using now, and `cx login` signs in to another account in the browser.

## Everyday use

| Command | What it does |
|---|---|
| `cx` | Live dashboard: fleet totals, usage bars, forecasts, history, settings, switching, all from the keyboard |
| `cx ls` | The same table, once. `--json` for scripts |
| `cx run` | Start Claude Code on the account with the most room, in this terminal only |
| `cx run work -- --resume` | The same on a named account; arguments after `--` go to `claude` |
| `cx use` | Move the default login to the best account. `cx use 3` for a specific one |
| `cx auto` | Watch usage and move the default login before it hits a limit |
| `cx service install` | Run `cx auto` in the background, started at login (macOS) |
| `cx why` | Show the ranking and what `cx auto` would do now, with reasons |
| `cx login 3` | Sign in again to an account whose refresh token has died |
| `cx map work` | Pin this directory to an account: a bare `cx run` here and below starts on it |
| `cx menubar` | The accounts in the macOS menu bar. `cx menubar install` starts it at login |
| `cx export backup.cx` | Every account, login and setting in one passphrase-encrypted file. `cx import` restores it |
| `cx statusline` | One line for Claude Code's status bar |
| `cx completions zsh` | Shell completion script |
| `cx doctor` | Check the setup |

An account is named by number, alias (`cx alias 3 work`), email, or any unambiguous part of one.

## The dashboard

`cx` alone opens it, and everything can be done from it: one screen, with pop-ups for settings, key entry and help. Rows and buttons take mouse clicks; `?` lists every key.

- **Subscriptions.** One row per account. Limits on all models (green bars) are kept apart from limits on a single model such as Fable (violet bars), in the table, in the status words and in the choice of account.
- **The selected account.** Each limit with what is used, what is left, when it resets and how fast it is rising. Below it, what used the account, today or over the last 7 days: which model, the main chat against subagents, which project, which tools, what was reused from the prompt cache against what was written or rebuilt, and how long hooks took.
- **Which account next.** The order cx would use the accounts in, once for Fable work and once for other models.
- **API keys.** A table with an "add" row. A key is typed or pasted into a pop-up and never shown; storing one turns on its use when every subscription is at its limit.
- **Settings** (`S`). A list; Enter opens an editor that fits the value: up and down for a number, a list for a choice, a checklist for the shared entries.

The breakdown is read from Claude Code's own transcripts under `~/.claude/projects`, which name the account each session ran on. Use from another computer or from claude.ai counts against an account's limits but leaves no record here. Shares are weighted the way the API prices tokens; the subscription limits are not published, so they are a yardstick and not the server's count. cx only reads the transcripts and keeps sums, never message text, in `~/.cx/activity.json`.

cx records a usage reading each time it checks, in `~/.cx/history.jsonl`, so rates and trends start from the day cx is first run.

### Fable and other models

An account can be out of its Fable week and still have room for every other model. The `model` setting says which single-model limit counts when cx chooses: `auto` (the default) counts the limit of the model Claude Code used last and ignores it when you are on another model; `all`, `none` or a model name fix it.

## How the balancing works

There are two ways to spread work, and they combine.

**`cx run` spreads sessions.** Each account has its own Claude Code profile, so sessions on different accounts run side by side. A bare `cx run` picks the account by the `strategy` setting:

- `balanced` (default): the most room left, less 12 points for every session already running on the account, plus up to 20 points for weekly quota that resets within 48 hours and would otherwise lapse unused.
- `headroom`: the most room left.
- `drain-soonest`: the account whose weekly window resets soonest and still has room.

Profiles share your settings, skills, plugins, hooks and conversation history with `~/.claude` by symlink, so `--resume` works across accounts.

**`cx auto` moves the default login.** It stays on the active account until a limit reaches its switch line, then moves to the best account. It does not move early: a conversation's prompt cache lives on the account that built it, so every switch re-sends the conversation at full price. A cooldown and a margin stop two accounts near the line from trading places.

**Room** is the number of percentage points left under the tightest switch line: 90% for the 5-hour window, 95% for the weekly window and for each per-model weekly limit (for example Fable's). Change them with `cx config`.

A directory pinned with `cx map <account>` overrides the choice for a bare `cx run` in it; naming an account on the command line overrides the pin.

API keys are billed per token, so they are never chosen automatically. `cx run <key>` uses one by name; `cx config api_fallback true` lets a bare `cx run` fall back to one when every subscription is at its limit.

## Sign-in and tokens

`cx login` runs Claude Code's own `claude auth login` in a throwaway config directory, moves the new login into cx, and deletes the directory. The default login is not disturbed.

An access token lasts a few hours and is renewed with a refresh token, which works once. cx refreshes on demand, under the same locks Claude Code holds for its own refresh, and writes the new token everywhere the account's login lives: cx's store, the default Claude Code home when the account is active, and the account's `cx run` profile. Before using an account it takes the newest copy from those places, so a token Claude Code refreshed itself is never lost. A rejected refresh token marks the account `needs sign-in`; `cx login <account>` fixes it.

## Backup and a second machine

`cx export <file>` asks for a passphrase and writes an [age](https://age-encryption.org) file (`age -d` opens it without cx). `cx import <file>` adds the accounts in it; an account already present keeps its number and is only overwritten by a newer login. `--settings` also brings settings and directory pins. `CX_PASSPHRASE` supplies the passphrase to scripts.

A refresh token works once. After a restore on a second machine, whichever machine refreshes an account first retires the other machine's copy of that login, and that copy then needs `cx login`.

## Status bar, menu bar, notifications

Claude Code's status bar, in `~/.claude/settings.json`:

```json
{ "statusLine": { "type": "command", "command": "cx statusline" } }
```

It prints the session's account and its limits (`#2 work · 5h 41% · wk 63%`) from the last readings, with no network call.

`cx menubar` puts the active account's fullest limit in the macOS menu bar, with a menu to switch, reload and turn automatic switching on.

`cx auto` (and automatic switching in the dashboard and menu bar) shows a desktop notification when it switches, when an account needs a new sign-in, and when every account is at its limit. `cx config notify off` stops them.

## The Claude Code plugin

The [cx plugin](https://github.com/protoworks-dev/claude-plugins) lets Claude Code read what cx measures and advise on its own setup: which cache rebuilds to stop, which settings to change, what should become a skill or a hook, and why you had to stop or correct it. Its hooks also tell cx about sessions as they run.

```sh
claude plugin marketplace add protoworks-dev/claude-plugins
claude plugin install cx@protoworks
```

The plugin reads two commands, which other tools can use too:

- `cx report` prints limits, usage and waste as JSON. It works with no account added to cx, from Claude Code's transcripts alone.
- `cx hook` takes a Claude Code hook event on standard input and records its name, session, project and a few numbers in `~/.cx/events.jsonl`. Prompt text, tool input and tool output are never written.

## Where things are

| | |
|---|---|
| State and settings | `~/.cx/state.json` |
| Logins | macOS Keychain, service `cx-accounts`. Without a Keychain: age files under `~/.cx/secrets` |
| `cx run` profiles | `~/.cx/profiles/<account>` |
| Usage history | `~/.cx/history.jsonl` |
| Sums from the transcripts | `~/.cx/activity.json` |
| Hook events from the plugin | `~/.cx/events.jsonl` |

Environment: `CX_HOME` moves the state directory, `CX_SECRETS` chooses where logins are kept (below), `CX_CLAUDE_BIN` names the `claude` binary.

### Where logins are kept

| `CX_SECRETS` | Store |
|---|---|
| unset, on macOS | The login Keychain |
| unset elsewhere, or `age` | One age file per login, `~/.cx/secrets/acct-<n>.age` |
| `fnox` | [fnox](https://fnox.jdx.dev), through the `fnox` command |
| `file` | Plain files that only you can read, not encrypted. For tests |

**age.** Each login is encrypted to an age key file, and `age -d -i <key file> ~/.cx/secrets/acct-1.age` opens one without cx. cx uses the first of these: `CX_AGE_KEY_FILE`; the file named by `FNOX_AGE_KEY_FILE`, `SOPS_AGE_KEY_FILE` or `MISE_SOPS_AGE_KEY_FILE`; an existing `~/.config/fnox/age.txt`, `~/.config/mise/age.txt` or `~/.config/sops/age/keys.txt`; and otherwise its own `~/.cx/age.key`, which it creates. So a machine that already keeps secrets with fnox, mise or sops uses that one key for cx too. `cx doctor` names the key in use. If the key file is lost the logins cannot be opened, and each account needs `cx login` again.

A key that cx made sits on the same disk as the logins, so it protects a backup or a synced copy of `~/.cx/secrets` that does not include the key, and not against someone who can read your whole home directory. Keep the key somewhere else (`CX_AGE_KEY_FILE`) for more than that.

**fnox.** `CX_SECRETS=fnox` keeps each login as a fnox secret named `CX_ACCT_<n>` in `~/.cx/fnox.toml` (`CX_FNOX_CONFIG` names another file). cx writes that file with one age provider on the key above. Edit it, or point `CX_FNOX_CONFIG` at a config of your own, to use any provider fnox has, such as a cloud secret manager or a password manager; `CX_FNOX_PROVIDER` picks a provider that is not the config's default. The value is handed to fnox on standard input, never on its command line.

Logins left by another store are moved the first time cx runs: plain files from before 0.6 into age, and plain or age files into fnox.

This is cx's own copy of each login. Claude Code keeps the login it is using in its own place, which outside macOS is a plain `.credentials.json` in its config directory; cx cannot change that.

## Settings

`cx config` lists them; `cx config <key> <value>` sets one.

| Key | Default | Meaning |
|---|---|---|
| `threshold_5h` | 90 | Switch line for the 5-hour window, in percent |
| `threshold_7d` | 95 | Switch line for the weekly and per-model windows |
| `model` | auto | Which single-model weekly limit counts: `auto` (the model in use), `all`, a model name, or `none` |
| `margin` | 10 | Points of room a new account must have over the current one |
| `cooldown_s` | 300 | Seconds between automatic switches |
| `interval_s` | 60 | Seconds between checks in `cx auto` |
| `strategy` | balanced | `balanced`, `headroom` or `drain-soonest` |
| `usage_ttl_s` | 120 | Seconds a usage reading stays fresh for the active account (three times that for the others) |
| `api_fallback` | false | Let a bare `cx run` use an API key when every subscription is at its limit |
| `notify` | true | Desktop notifications from automatic switching |
| `keep_awake` | false | With the plugin: keep the Mac awake for 80 minutes after a turn on a large conversation, so Claude Code summarises it before its cache expires |
| `share` | settings, skills, plugins, hooks, history… | Entries of `~/.claude` a profile shares by symlink |

## Development

```bash
cargo test                                             # unit and end-to-end tests
cargo test --lib keychain_round_trip -- --ignored      # the real Keychain, on a scratch item
```

The end-to-end tests run the real binary against a temporary home directory, file-backed secrets and a local stand-in for Anthropic's endpoints. They cannot reach the real Keychain, the real `~/.claude`, or the network.

`src/engine.rs` holds the rules for moving credentials; read its header before changing anything that writes one.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the rules on changes and [SECURITY.md](SECURITY.md) for reporting a vulnerability.

## Licence

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
