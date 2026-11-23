//! Several Claude accounts behind one Claude Code.
//!
//! `claude-cx` keeps a login for each of your Claude subscription accounts
//! and API keys, shows how much of each one's limits is used, says what
//! used them, and puts your work on the account with the most room. It
//! installs one command, `cx`, whose dashboard does everything.
//!
//! This crate is the library behind that command. It is published so the
//! code is documented and testable in pieces; the command is the supported
//! interface, and the library's API may change between minor versions.
//!
//! # Map
//!
//! | Module | What it holds |
//! |---|---|
//! | [`store`] | Accounts, settings and the last usage readings, in `state.json` |
//! | [`secrets`] | Logins and keys: the macOS Keychain, or age-encrypted files, or fnox |
//! | [`cred`] | The shape of a Claude Code login |
//! | [`claude`] | Claude Code's own files, Keychain items and locks |
//! | [`api`] | The three Anthropic endpoints cx calls |
//! | [`engine`] | The rules for refreshing, converging and switching logins |
//! | [`rank`] | Which account is best, and when to switch |
//! | [`history`] | Usage readings over time, and forecasts from them |
//! | [`activity`] | What each account was used for, from Claude Code's transcripts |
//! | [`report`] | Everything cx knows as JSON, for the Claude Code plugin |
//! | [`hook`] | What Claude Code's hooks tell cx during a session |
//! | [`plugin`] | Installing the cx plugin for Claude Code |
//! | [`run`] | Starting Claude Code on one account in one terminal |
//! | [`accounts`] | Adding accounts: sign-in, API keys, import from claude-swap |
//! | [`transfer`] | Encrypted export and import |
//! | [`tui`] | The dashboard |
//! | [`ui`] | Plain-terminal output for the one-shot commands |
//! | [`service`] | Running in the background under launchd |
//! | [`notify`] | Desktop notifications |
//! | [`lock`], [`util`] | Directory locks, atomic files, small helpers |
//!
//! # Safety of credentials
//!
//! A refresh token works once. The rules that keep one from being lost are
//! in the header of [`engine`]; read it before changing anything that
//! writes a login.

#![warn(missing_docs)]

pub mod accounts;
pub mod activity;
pub mod api;
pub mod claude;
pub mod cred;
pub mod engine;
pub mod history;
pub mod hook;
pub mod lock;
#[cfg(target_os = "macos")]
pub mod menubar;
pub mod notify;
pub mod plugin;
pub mod rank;
pub mod report;
pub mod run;
pub mod secrets;
pub mod service;
pub mod store;
pub mod transfer;
pub mod tui;
pub mod ui;
pub mod util;
