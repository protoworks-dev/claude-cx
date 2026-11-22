# Security policy

claude-cx stores and moves login credentials for Claude accounts. Reports about anything that could expose, lose or misuse a credential are taken seriously.

## Reporting a vulnerability

Report privately through GitHub: open the repository's **Security** tab and choose **Report a vulnerability**
(<https://github.com/protoworks-dev/claude-cx/security/advisories/new>).

Do not open a public issue or pull request for a vulnerability, and do not include real tokens, API keys, email addresses or account ids in a report. Redact them; a description of the shape is enough.

Please include:

- the version (`cx --version`) and the operating system;
- what an attacker can do and what they need to do it (local user, another process, a crafted file, network position);
- steps to reproduce, with a scratch `CX_HOME` and `CX_SECRETS=file` where possible.

## What to expect

- An acknowledgement within 3 working days.
- An assessment, and a plan if we confirm it, within 10 working days.
- A fix released as a new version, with credit to you in the advisory unless you ask otherwise.
- Coordinated disclosure: we ask for up to 90 days before details are published.

## Supported versions

Only the latest release on crates.io receives security fixes.

## Scope

In scope:

- credentials written somewhere another local user or process can read them;
- a credential, key or passphrase appearing in logs, on screen, in `state.json`, `history.jsonl` or `activity.json`;
- a refresh token lost or duplicated by a race, a crash or an error path;
- an export file that can be opened without its passphrase;
- a crafted file (transcript, state, export, claude-swap store) that makes cx run code or write outside its own directories.

Out of scope:

- anything that requires the attacker to already be you on your own machine with your Keychain unlocked;
- Anthropic's services, Claude Code itself, and the terms on which accounts may be used. Report those to Anthropic.

## How credentials are handled

- Logins and keys are in the macOS Keychain (service `cx-accounts`), or in owner-only files (mode 0600) when `CX_SECRETS=file` or the platform has no Keychain.
- `state.json` holds no secret. A token is identified in it by a truncated SHA-256 fingerprint.
- Transcripts under `~/.claude` are only read. cx keeps sums from them, never message text.
- Network calls go only to `api.anthropic.com` and `platform.claude.com`, over TLS.
