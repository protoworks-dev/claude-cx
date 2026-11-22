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

The end-to-end tests run the real binary against a temporary home, file-backed secrets and a local stand-in for Anthropic's endpoints. They cannot reach your Keychain, your `~/.claude` or the network. Keep it that way: a test must never need a real account.

To try a build by hand without touching your real accounts:

```bash
CX_HOME=/tmp/cx-scratch CX_SECRETS=file cargo run
```

## Rules for changes

- **Credentials.** Read the header of `src/engine.rs` before changing anything that reads or writes a login. Keep its lock order and its rule that a token reply is stored before anything else happens. A change there needs a test that fails without it.
- **Nothing secret in output.** No token, key or passphrase in logs, errors, the screen or any file other than the secret store.
- **Documentation.** Every public item has a doc comment that says what a caller needs to know. `#![warn(missing_docs)]` is on and CI treats warnings as errors.
- **Naming.** Rust API Guidelines: `snake_case` functions and modules, `UpperCamelCase` types, no `get_` prefixes, conversions named `as_`/`to_`/`into_`. Times carry their unit in the name (`_ms`, `_s`).
- **Words on screen.** Plain words, labelled numbers, single-colour bars. The dashboard must stay readable without relying on colour gradients, and must never tell the user to go and run a command.
- **Dependencies.** Keep them current and few. Say in the pull request why a new one is needed.
- **Formatting.** `cargo fmt` with the repository's `rustfmt.toml`.

## Commits and pull requests

- One logical change per pull request, with tests.
- Describe what changes for a user, and how you checked it.
- Add a line to `CHANGELOG.md` under "Unreleased".

## Licence

By contributing you agree that your contribution is licensed under the Apache License 2.0, as in [LICENSE](LICENSE).
