# Changelog

All notable changes are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow [Semantic Versioning](https://semver.org/).

## Unreleased

### Security

- Shared entries must be plain names. An import file or a hand-edited state file can no longer make a profile link to, or create a link at, a path outside `~/.claude` and the profile.
- A credential too long for the `security` tool's standard input is no longer passed on its command line, where other users could read it from the process list. It is kept in an owner-only file instead.

## 0.4.0 - 2026-11-22

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
