# Changelog

All notable changes are recorded here. The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0] - 2026-11-23

### Added

- `cx help` lists the commands under headings by what they are for, and says where to start. Bare `cx` with no accounts yet prints the same starting points instead of an empty dashboard.
- `cx status` says which account Claude Code is on, what the current directory is pinned to, and which account cx would choose. `--json` for scripts.
- `cx threshold <account> <percent>` gives one account its own switch line, in place of the two in the settings. Without a number it goes back to the settings. `cx why` names an account's own line.
- `cx upgrade` (also `cx update`) replaces cx with the latest release after checking the download against the release's SHA-256. `--check` only says whether a newer release exists.
- After `cx import-cswap`, cx names the command that stops claude-swap's background refresh.

### Changed

- The changelog follows Keep a Changelog 1.1.0 to the letter: bracketed versions that link to their comparison, and one section per kind of change. A test checks it.
- Commits and pull request titles follow Conventional Commits 1.0.0. `scripts/check-commits.sh` checks them, as a `commit-msg` hook and in CI.

## [0.6.0] - 2026-11-23

### Added

- Prebuilt binaries for macOS and Linux, on Apple and Intel/AMD and ARM chips, on each GitHub release. `install.sh` fetches the right one and checks its SHA-256; `cargo binstall claude-cx` uses them too. Installing no longer needs a Rust toolchain. `scripts/dist.sh` builds them: macOS with cargo, Linux in a Debian 11 container, where the tests also run.
- `cx plugin` installs the cx plugin for Claude Code in one command: it adds the Protoworks marketplace and installs the plugin, or updates both. `cx plugin status` and `cx plugin remove` go with it, and `cx doctor` says whether the plugin is there.
- Screenshots in the README, made from the real program on made-up accounts by `scripts/screenshots.py`.
- `CX_SECRETS=fnox` keeps the logins in fnox, with any provider fnox has. `cx doctor` says where the logins are and which age key opens them.
- More is read from the transcripts, all of it kept as counts on this machine: thinking tokens by effort level, cache writes by lifetime, how much text each tool put into conversations and how often it failed, time and added text per hook command, hooks that blocked or were cancelled, refused tool calls, work done under each skill, plugin and MCP server, API errors, turn times, the size of the lists sent at every session start, and summaries made while idle. `cx report` carries them all.
- The dashboard's cards are on two pages, "Usage" and "Waste and speed" (`v`, or click). A line above the cards names the largest loss of tokens and of time. Cards carry their headline figure in the title.
- `keep_awake` setting (off by default, macOS). With the plugin's hooks, the end of a turn on a large conversation holds the Mac awake for 80 minutes, so Claude Code can summarise the conversation before its cache expires. Sending a prompt or ending the session releases it. Without this, a Mac that sleeps first pays for the whole conversation again on the next prompt.

### Changed

- CI runs on a runner named by the `CI_RUNNER` repository variable (the organisation's own runners when unset), and installs the package the way a user does. It no longer runs for pull requests from forks.

### Fixed

- Switching or renewing a login no longer signs Claude Code out when its Keychain item is large. Since 0.5.0, a login that also held MCP server sign-ins (anything too long for the `security` tool's standard input) was moved to the plaintext file and the Keychain item removed; Claude Code on macOS reads only the Keychain item, so it was left signed out. The item now stays in the Keychain. 0.4.0 is not affected.

### Security

- On a system without a Keychain (Linux), logins and API keys are now encrypted with age instead of being kept in plain files. The key is the one fnox, mise or sops already uses when there is one, else a key cx makes in `~/.cx/age.key`. Plain files left by 0.5 are encrypted the first time cx runs. `CX_SECRETS=file` keeps the old behaviour.

## [0.5.0] - 2026-11-22

### Added

- A "Wasted work" card on the dashboard: how often you pressed Escape and how much Claude had written first, how often you corrected it, how often the chat was summarised, and how many tokens went on rebuilding the prompt cache, with the main cause.
- Cache rebuilds are told apart by cause: the start of a conversation (unavoidable), a pause of over five minutes or over an hour, a summary, a change of model, or something else; and whether it happened in a subagent, whose cache is kept for a shorter time.
- A "Skills started" card, and a list of files edited over and over in one conversation.
- `cx report`: all of the above as JSON, for the Claude Code plugin and other tools. It works with no account added. `cx report --moments <transcript>` shows what was asked, what Claude was doing and what you typed next at each stop or correction.
- `cx hook`: records Claude Code hook events (names, kinds and numbers; never prompt text, tool input or output) and says when a prompt is sent on an expired cache, with the size that will be read again.

### Changed

- The cache note counts only rebuilds in the middle of a conversation; the first reply of a conversation always writes the cache and is no longer counted as a rebuild to worry about.

## [0.4.0] - 2026-11-22

First public release.

### Added

- A one-screen dashboard (`cx`): subscriptions with their limits, the selected account in detail, which account is next, API keys, and a log of what cx did. Rows and buttons take mouse clicks.
- Limits on all models kept apart from limits on a single model, on screen and when choosing an account.
- A breakdown of what used each account, read from Claude Code's transcripts: model, main chat against subagents, project, tools, prompt-cache reuse and rebuilds, hook time, output per day.
- Sign-in through Claude Code's own browser flow, token renewal, and convergence of the copies of a login.
- Account choice by room under each limit, with automatic switching and a per-terminal `cx run`.
- API keys, entered in a pop-up and used only when every subscription is at its limit.
- Settings as a submenu with an editor for each kind of value.
- Directory pins, encrypted export and import, a macOS menu-bar item, desktop notifications, a status line for Claude Code, shell completions, and import from claude-swap.

### Security

- Shared entries must be plain names. An import file or a hand-edited state file can no longer make a profile link to, or create a link at, a path outside `~/.claude` and the profile.
- A credential too long for the `security` tool's standard input is no longer passed on its command line, where other users could read it from the process list. It is kept in an owner-only file instead.

[Unreleased]: https://github.com/protoworks-dev/claude-cx/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/protoworks-dev/claude-cx/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/protoworks-dev/claude-cx/compare/23f03ad3b07a2d8645e059c3d19ec1b349f68987...v0.6.0
[0.5.0]: https://github.com/protoworks-dev/claude-cx/compare/797674c8211268a8b040b37f04cda711c229caf7...23f03ad3b07a2d8645e059c3d19ec1b349f68987
[0.4.0]: https://github.com/protoworks-dev/claude-cx/commit/797674c8211268a8b040b37f04cda711c229caf7
