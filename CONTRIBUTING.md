# Contributing

Thanks for looking. This is a small tool with one sharp edge: it moves login credentials. Most of the rules below exist because a mistake there costs someone a sign-in.

## Before you start

- Open an issue first for anything larger than a fix, so the approach can be agreed.
- Security problems go through [SECURITY.md](SECURITY.md), not issues.

## Building and testing

```bash
cargo test                    # unit and end-to-end tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo doc --no-deps           # rustdoc must build without warnings
```

To check what would be published, build the package into its own directory: `cargo package --target-dir /tmp/claude-cx-package`. Verifying into the normal `target/` leaves artifacts that cargo then treats as the current build, and the end-to-end tests run against that snapshot until `cargo clean -p claude-cx`.

The end-to-end tests run the real binary against a temporary home, file-backed secrets and a local stand-in for Anthropic's endpoints. They cannot reach your Keychain, your `~/.claude` or the network. Keep it that way: a test must never need a real account.

To try a build by hand without touching your real accounts:

```bash
CX_HOME=/tmp/cx-scratch CX_SECRETS=file cargo run
```

## Releasing

1. Move the entries under `[Unreleased]` in `CHANGELOG.md` to a new `## [x.y.z] - YYYY-MM-DD` section, add its link at the foot of the file, and set the same version in `Cargo.toml` (`cargo check` updates `Cargo.lock`). The version follows Semantic Versioning: a `feat` raises the minor number, a `fix` the patch number, and a breaking change the major number (the minor number while the version starts with 0). Commit as `chore(release): x.y.z`.
2. On that commit on `main`, run `scripts/release.sh`. It builds the four release archives with `scripts/dist.sh` (macOS with cargo on a Mac, Linux in a Debian 11 container, where the tests also run), creates the GitHub release `vx.y.z` with the archives, `SHA256SUMS` and the version's changelog section as its notes, and publishes the crate when cargo has a crates.io token. `--dry-run` builds and checks without creating anything.

`install.sh`, `cx upgrade` and `cargo binstall` fetch the archives by name from the latest release, so never create a release by hand without them.

## Rules for changes

- **Credentials.** Read the header of `src/engine.rs` before changing anything that reads or writes a login. Keep its lock order and its rule that a token reply is stored before anything else happens. A change there needs a test that fails without it.
- **Nothing secret in output.** No token, key or passphrase in logs, errors, the screen or any file other than the secret store.
- **Documentation.** Every public item has a doc comment that says what a caller needs to know. `#![warn(missing_docs)]` is on and CI treats warnings as errors.
- **Naming.** Rust API Guidelines: `snake_case` functions and modules, `UpperCamelCase` types, no `get_` prefixes, conversions named `as_`/`to_`/`into_`. Times carry their unit in the name (`_ms`, `_s`).
- **Words on screen.** Plain words, labelled numbers, single-colour bars. The dashboard must stay readable without relying on colour gradients, and must never tell the user to go and run a command.
- **Dependencies.** Keep them current and few. Say in the pull request why a new one is needed.
- **Formatting.** `cargo fmt` with the repository's `rustfmt.toml`.

## Commits and pull requests

Commit subjects and pull request titles follow [Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/):

```
<type>[(scope)][!]: <description>
```

- **Types:** `feat` (something a user can now do), `fix` (something that was wrong), `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `revert`.
- **Scope**, optional: the module or area, such as `fix(secrets): …` or `feat(tui): …`.
- **Description:** what the change does, starting lower case, with no full stop, in 72 characters or fewer with the rest of the subject.
- **Breaking change:** `!` before the colon, and a `BREAKING CHANGE:` footer that says what a user must do.

`scripts/check-commits.sh` checks this. Turn it on as a `commit-msg` hook in your clone with `git config core.hooksPath .githooks`; CI runs it on every pull request, title included, because a pull request is squashed and its title becomes the commit on `main`.

- One logical change per pull request, with tests.
- Describe what changes for a user, and how you checked it.
- Add an entry to `CHANGELOG.md` under `[Unreleased]`, in the section for its kind: Added, Changed, Deprecated, Removed, Fixed or Security, in that order ([Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/)). Write it for a user of cx, not for a reader of the diff. `cargo test` checks the file's form.

## Licence

By contributing you agree that your contribution is licensed under the Apache License 2.0, as in [LICENSE](LICENSE).
