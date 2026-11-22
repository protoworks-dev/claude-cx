## What changes for a user

## How it was checked

- [ ] `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `cargo doc --no-deps`
- [ ] No token, key, email address or account id in the diff, the tests or this description
- [ ] If it touches a login: the rules in the header of `src/engine.rs` still hold, and a test fails without the change
- [ ] `CHANGELOG.md` updated
