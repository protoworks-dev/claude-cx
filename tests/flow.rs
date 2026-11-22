//! End-to-end tests: the real `cx` binary against a throwaway home directory,
//! file-backed secrets and a local stand-in for Anthropic's endpoints.
//! Nothing here can reach the real Keychain, the real `~/.claude`, or the
//! network: every path hangs off a temporary `HOME` and every URL is local.

use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

const HOUR_MS: i64 = 3_600_000;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[derive(Default)]
struct Server {
    /// account letter -> (5-hour %, weekly %)
    usage: HashMap<String, (f64, f64)>,
    spent_refresh_tokens: HashSet<String>,
    refreshes: usize,
    usage_reads: usize,
    /// Accounts whose usage endpoint answers 429.
    rate_limited: HashSet<String>,
    /// Answer a refresh with 200 and a body in an unknown shape.
    garbled_token_replies: bool,
}

/// Tokens look like `at:A:3` and `rt:A:3`: kind, account letter, generation.
fn parts(token: &str) -> Option<(String, String, u32)> {
    let mut it = token.split(':');
    Some((it.next()?.to_string(), it.next()?.to_string(), it.next()?.parse().ok()?))
}

fn respond(stream: &mut std::net::TcpStream, status: &str, body: &Value) {
    let body = body.to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn serve(state: Arc<Mutex<Server>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            let mut headers = HashMap::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    headers.insert(name.trim().to_lowercase(), value.trim().to_string());
                }
            }
            let length: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
            let mut body = vec![0u8; length];
            let _ = reader.read_exact(&mut body);
            let path = request_line.split_whitespace().nth(1).unwrap_or("").to_string();
            let bearer = headers
                .get("authorization")
                .and_then(|v| v.strip_prefix("Bearer "))
                .and_then(parts);
            let mut server = state.lock().unwrap();
            match path.as_str() {
                "/v1/oauth/token" => {
                    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let token = request["refresh_token"].as_str().unwrap_or("").to_string();
                    match parts(&token) {
                        Some((kind, account, generation))
                            if kind == "rt" && !server.spent_refresh_tokens.contains(&token) =>
                        {
                            server.spent_refresh_tokens.insert(token);
                            server.refreshes += 1;
                            if server.garbled_token_replies {
                                respond(
                                    &mut stream,
                                    "200 OK",
                                    &json!({"token": {"value": format!("at:{account}:{}", generation + 1),
                                    "renew": format!("rt:{account}:{}", generation + 1)}}),
                                );
                                continue;
                            }
                            respond(
                                &mut stream,
                                "200 OK",
                                &json!({
                                    "access_token": format!("at:{account}:{}", generation + 1),
                                    "refresh_token": format!("rt:{account}:{}", generation + 1),
                                    "expires_in": 3600,
                                    "scope": "user:inference user:profile",
                                    "account": {"uuid": format!("uuid-{account}"), "email_address": format!("{account}@example.test")},
                                    "organization": {"uuid": format!("org-{account}")}
                                }),
                            );
                        }
                        _ => respond(&mut stream, "400 Bad Request", &json!({"error": "invalid_grant"})),
                    }
                }
                "/api/oauth/usage" => match bearer {
                    Some((kind, account, _)) if kind == "at" => {
                        server.usage_reads += 1;
                        if server.rate_limited.contains(&account) {
                            respond(&mut stream, "429 Too Many Requests", &json!({}));
                            continue;
                        }
                        let (five, week) = server.usage.get(&account).copied().unwrap_or((0.0, 0.0));
                        let later = |hours: i64| {
                            chrono::DateTime::from_timestamp_millis(now_ms() + hours * HOUR_MS)
                                .unwrap()
                                .to_rfc3339()
                        };
                        respond(
                            &mut stream,
                            "200 OK",
                            &json!({
                                "five_hour": {"utilization": five, "resets_at": later(2)},
                                "seven_day": {"utilization": week, "resets_at": later(90)},
                            }),
                        );
                    }
                    _ => respond(&mut stream, "401 Unauthorized", &json!({})),
                },
                "/api/oauth/profile" => match bearer {
                    Some((kind, account, _)) if kind == "at" => respond(
                        &mut stream,
                        "200 OK",
                        &json!({
                            "account": {"uuid": format!("uuid-{account}"), "email": format!("{account}@example.test")},
                            "organization": {"uuid": format!("org-{account}"), "name": format!("{account} org"),
                                             "organization_type": "claude_max", "rate_limit_tier": "default_claude_max_20x"}
                        }),
                    ),
                    _ => respond(&mut stream, "401 Unauthorized", &json!({})),
                },
                _ => respond(&mut stream, "404 Not Found", &json!({})),
            }
        }
    });
    address
}

struct World {
    home: PathBuf,
    server: Arc<Mutex<Server>>,
    address: String,
}

struct Ran {
    code: i32,
    out: String,
    err: String,
}

fn credential(account: &str, generation: u32, expires_ms: i64) -> Value {
    json!({"claudeAiOauth": {
        "accessToken": format!("at:{account}:{generation}"),
        "refreshToken": format!("rt:{account}:{generation}"),
        "expiresAt": expires_ms,
        "scopes": ["user:inference", "user:profile"],
        "subscriptionType": "max"
    }})
}

fn oauth_account(account: &str) -> Value {
    json!({"accountUuid": format!("uuid-{account}"), "emailAddress": format!("{account}@example.test"),
           "organizationUuid": format!("org-{account}"), "organizationName": format!("{account} org"), "displayName": account})
}

fn read(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display())))
        .unwrap()
}

fn write(path: &Path, value: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, value.to_string()).unwrap();
}

impl World {
    fn new() -> World {
        let home = std::env::temp_dir().join(format!("cx-test-{}-{}", std::process::id(), fastrand::u64(..)));
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let server = Arc::new(Mutex::new(Server::default()));
        let address = serve(server.clone());
        World { home, server, address }
    }

    fn cx(&self, args: &[&str]) -> Ran {
        self.cx_with(args, &[])
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cx"));
        command
            .args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap())
            .env("HOME", &self.home)
            .env("USER", "tester")
            .env("CX_HOME", self.home.join(".cx"))
            .env("CX_SECRETS", "file")
            .env("CX_API_BASE", &self.address)
            .env("CX_TOKEN_URL", format!("{}/v1/oauth/token", self.address))
            .env("CX_CLAUDE_BIN", self.home.join("fake-claude"))
            .env("CX_NOTIFY_LOG", self.home.join("notify.log"))
            .env("NO_COLOR", "1");
        command
    }

    fn cx_with(&self, args: &[&str], env: &[(&str, &str)]) -> Ran {
        self.finish(self.command(args).envs(env.iter().copied()))
    }

    /// Run cx from inside a directory.
    fn cx_in(&self, dir: &Path, args: &[&str]) -> Ran {
        self.finish(self.command(args).current_dir(dir))
    }

    fn finish(&self, command: &mut Command) -> Ran {
        let output = command.output().unwrap();
        Ran {
            code: output.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&output.stdout).into_owned(),
            err: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn ok(&self, args: &[&str]) -> Ran {
        let ran = self.cx(args);
        assert_eq!(
            ran.code, 0,
            "cx {args:?} failed\nstdout: {}\nstderr: {}",
            ran.out, ran.err
        );
        ran
    }

    fn live_file(&self) -> PathBuf {
        self.home.join(".claude").join(".credentials.json")
    }

    fn config_file(&self) -> PathBuf {
        self.home.join(".claude.json")
    }

    /// Put a login into the default Claude Code home, as `/login` would.
    fn sign_in_claude(&self, account: &str, generation: u32, expires_ms: i64) {
        let mut cred = credential(account, generation, expires_ms);
        cred["mcpOAuth"] = json!({"some-server": {"token": "machine-wide"}});
        write(&self.live_file(), &cred);
        let mut config = if self.config_file().exists() {
            read(&self.config_file())
        } else {
            json!({"theme": "dark", "numStartups": 7})
        };
        config["oauthAccount"] = oauth_account(account);
        write(&self.config_file(), &config);
    }

    fn live_access(&self) -> String {
        read(&self.live_file())["claudeAiOauth"]["accessToken"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn secret(&self, id: u32) -> Value {
        read(&self.home.join(".cx").join("secrets").join(format!("acct-{id}.secret")))
    }

    fn set_secret(&self, id: u32, value: &Value) {
        std::fs::write(
            self.home.join(".cx").join("secrets").join(format!("acct-{id}.secret")),
            value.to_string(),
        )
        .unwrap();
    }

    fn set_usage(&self, account: &str, five: f64, week: f64) {
        self.server
            .lock()
            .unwrap()
            .usage
            .insert(account.to_string(), (five, week));
    }

    fn list(&self, extra: &[&str]) -> Value {
        let mut args = vec!["ls", "--json"];
        args.extend_from_slice(extra);
        serde_json::from_str(&self.ok(&args).out).unwrap()
    }

    /// Two accounts, A as #1 and B as #2, with B left as the default login.
    fn with_two_accounts() -> World {
        let world = World::new();
        world.sign_in_claude("A", 1, now_ms() + 8 * HOUR_MS);
        world.ok(&["add"]);
        world.sign_in_claude("B", 1, now_ms() + 8 * HOUR_MS);
        world.ok(&["add"]);
        world
    }

    /// A stand-in for the `claude` binary: records how it was started, and
    /// for `auth login` leaves a login for account C in its config directory.
    fn install_fake_claude(&self) {
        let script = format!(
            r#"#!/bin/sh
if [ "$1" = "auth" ] && [ "$2" = "login" ]; then
  printf '%s' '{}' > "$CLAUDE_CONFIG_DIR/.credentials.json"
  printf '%s' '{}' > "$CLAUDE_CONFIG_DIR/.claude.json"
  exit 0
fi
echo "config_dir=${{CLAUDE_CONFIG_DIR:-default}}"
echo "api_key=${{ANTHROPIC_API_KEY:-none}}"
echo "account=${{CX_ACCOUNT:-none}}"
echo "args=$*"
"#,
            credential("C", 1, now_ms() + 8 * HOUR_MS),
            json!({"oauthAccount": oauth_account("C")}),
        );
        let path = self.home.join("fake-claude");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn account(list: &Value, id: u64) -> &Value {
    list["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == id)
        .unwrap_or_else(|| panic!("no account {id} in {list}"))
}

#[test]
fn add_takes_over_the_current_login_and_does_not_duplicate() {
    let world = World::with_two_accounts();
    world.ok(&["add"]);
    let list = world.list(&[]);
    assert_eq!(list["accounts"].as_array().unwrap().len(), 2);
    assert_eq!(account(&list, 1)["label"], "A@example.test");
    assert_eq!(account(&list, 2)["label"], "B@example.test");
    assert_eq!(account(&list, 1)["plan"], "Max 20x");
    assert_eq!(list["activeAccount"], 2);
    // The stored secret holds the login only, not the machine-wide members.
    assert_eq!(world.secret(1)["claudeAiOauth"]["refreshToken"], "rt:A:1");
    assert!(world.secret(1).get("mcpOAuth").is_none());
}

#[test]
fn list_reads_usage_and_caches_it() {
    let world = World::with_two_accounts();
    world.set_usage("A", 42.0, 17.0);
    world.set_usage("B", 5.0, 88.0);
    let list = world.list(&[]);
    assert_eq!(account(&list, 1)["usage"]["five_hour"]["pct"], 42.0);
    assert_eq!(account(&list, 2)["usage"]["seven_day"]["pct"], 88.0);
    let reads = world.server.lock().unwrap().usage_reads;
    // A second listing inside the freshness window asks the server nothing.
    world.list(&[]);
    assert_eq!(world.server.lock().unwrap().usage_reads, reads);
    world.list(&["--fresh"]);
    assert_eq!(world.server.lock().unwrap().usage_reads, reads + 2);
    let table = world.ok(&["ls"]).out;
    assert!(table.contains("A@example.test") && table.contains("42%"), "{table}");
}

#[test]
fn use_switches_the_login_and_leaves_everything_else_alone() {
    let world = World::with_two_accounts();
    world.ok(&["use", "1"]);
    assert_eq!(world.live_access(), "at:A:1");
    let live = read(&world.live_file());
    assert_eq!(live["mcpOAuth"]["some-server"]["token"], "machine-wide");
    let config = read(&world.config_file());
    assert_eq!(config["oauthAccount"]["accountUuid"], "uuid-A");
    assert_eq!(config["oauthAccount"]["displayName"], "A");
    assert_eq!((&config["theme"], &config["numStartups"]), (&json!("dark"), &json!(7)));
    assert_eq!(world.list(&[])["activeAccount"], 1);
    // By email fragment, and back.
    world.ok(&["use", "B@"]);
    assert_eq!(world.live_access(), "at:B:1");
    assert!(world.ok(&["use", "2"]).out.contains("already the default login"));
}

#[test]
fn an_expired_token_is_refreshed_and_written_everywhere_it_lives() {
    let world = World::with_two_accounts();
    // B is the default login. Expire it in both places.
    world.sign_in_claude("B", 1, now_ms() - HOUR_MS);
    world.set_secret(2, &credential("B", 1, now_ms() - HOUR_MS));
    world.list(&["--fresh"]);
    assert_eq!(world.secret(2)["claudeAiOauth"]["refreshToken"], "rt:B:2");
    assert_eq!(world.live_access(), "at:B:2");
    assert_eq!(
        read(&world.live_file())["mcpOAuth"]["some-server"]["token"],
        "machine-wide"
    );
    assert_eq!(world.server.lock().unwrap().refreshes, 1);
    // The refreshed login is still recognised as account 2.
    assert_eq!(world.list(&[])["activeAccount"], 2);
}

#[test]
fn a_token_claude_code_refreshed_itself_is_brought_home_before_a_switch() {
    let world = World::with_two_accounts();
    // Claude Code refreshed B on its own: cx has never seen rt:B:5.
    world.sign_in_claude("B", 5, now_ms() + 9 * HOUR_MS);
    world.ok(&["use", "1"]);
    assert_eq!(world.live_access(), "at:A:1");
    assert_eq!(world.secret(2)["claudeAiOauth"]["refreshToken"], "rt:B:5");
    // The login cx held before is kept beside it, not lost.
    let previous = read(&world.home.join(".cx/secrets/acct-2.prev.secret"));
    assert_eq!(previous["claudeAiOauth"]["refreshToken"], "rt:B:1");
    world.ok(&["use", "2"]);
    assert_eq!(world.live_access(), "at:B:5");
}

#[test]
fn a_dead_refresh_token_marks_the_account_and_auto_leaves_it() {
    let world = World::with_two_accounts();
    world
        .server
        .lock()
        .unwrap()
        .spent_refresh_tokens
        .insert("rt:B:1".into());
    world.sign_in_claude("B", 1, now_ms() - HOUR_MS);
    world.set_secret(2, &credential("B", 1, now_ms() - HOUR_MS));
    let refresh = world.cx(&["refresh", "2"]);
    assert_eq!(refresh.code, 1);
    assert!(refresh.err.contains("cx login 2"), "{}", refresh.err);
    let list = world.list(&[]);
    assert_eq!(account(&list, 2)["needsLogin"], true);
    assert_eq!(account(&list, 2)["eligible"], false);
    // B is the default login and cannot work, so auto moves to A.
    let auto = world.cx(&["auto", "--once"]);
    assert_eq!(auto.code, 0, "{} {}", auto.out, auto.err);
    assert_eq!(world.live_access(), "at:A:1");
}

#[test]
fn auto_stays_until_the_line_then_moves_to_the_account_with_most_room() {
    let world = World::with_two_accounts();
    world.sign_in_claude("C", 1, now_ms() + 8 * HOUR_MS);
    world.ok(&["add"]);
    world.ok(&["use", "1"]);
    world.set_usage("A", 80.0, 20.0);
    world.set_usage("B", 40.0, 20.0);
    world.set_usage("C", 10.0, 20.0);
    let stay = world.cx(&["auto", "--once"]);
    assert_eq!(stay.code, 2, "{}", stay.out);
    assert!(stay.out.contains("staying on #1"), "{}", stay.out);

    world.set_usage("A", 93.0, 20.0);
    world.ok(&["ls", "--fresh"]);
    let dry = world.cx(&["auto", "--once", "--dry-run"]);
    assert!(dry.out.contains("would switch to #3"), "{}", dry.out);
    assert_eq!(world.live_access(), "at:A:1");
    let moved = world.cx(&["auto", "--once"]);
    assert_eq!(moved.code, 0, "{} {}", moved.out, moved.err);
    assert_eq!(world.live_access(), "at:C:1");

    // A disabled account is skipped; priority outranks room.
    world.ok(&["use", "1"]);
    world.ok(&["disable", "3"]);
    assert!(world.ok(&["why"]).out.contains("disabled"));
    world.ok(&["use"]);
    assert_eq!(world.live_access(), "at:B:1");
}

#[test]
fn a_rate_limited_usage_read_keeps_the_last_numbers() {
    let world = World::with_two_accounts();
    world.set_usage("A", 33.0, 12.0);
    world.list(&[]);
    world.server.lock().unwrap().rate_limited.insert("A".into());
    let list = world.list(&["--fresh"]);
    assert_eq!(account(&list, 1)["usage"]["five_hour"]["pct"], 33.0);
    assert_eq!(account(&list, 1)["usageError"], "rate limited");
    // Backed off: even --fresh does not ask again straight away.
    let reads = world.server.lock().unwrap().usage_reads;
    world.list(&["--fresh"]);
    assert_eq!(
        world.server.lock().unwrap().usage_reads,
        reads + 1,
        "only B is read again"
    );
}

#[test]
fn run_starts_claude_in_the_accounts_own_profile() {
    let world = World::with_two_accounts();
    world.install_fake_claude();
    std::fs::write(world.home.join(".claude/settings.json"), "{}").unwrap();
    std::fs::create_dir_all(world.home.join(".claude/skills")).unwrap();
    write(
        &world.config_file(),
        &json!({"oauthAccount": oauth_account("B"), "theme": "dark",
        "mcpServers": {"docs": {"command": "x"}}, "projects": {"/work": {"hasTrustDialogAccepted": true}},
        "primaryApiKey": "must-not-be-copied"}),
    );

    let ran = world.ok(&["run", "1", "--", "--resume", "abc"]);
    let profile = world.home.join(".cx/profiles/1");
    assert!(
        ran.out.contains(&format!("config_dir={}", profile.display())),
        "{}",
        ran.out
    );
    assert!(
        ran.out.contains("args=--resume abc") && ran.out.contains("account=1"),
        "{}",
        ran.out
    );
    assert_eq!(
        read(&profile.join(".credentials.json"))["claudeAiOauth"]["accessToken"],
        "at:A:1"
    );
    let config = read(&profile.join(".claude.json"));
    assert_eq!(config["oauthAccount"]["accountUuid"], "uuid-A");
    assert_eq!(config["theme"], "dark");
    assert_eq!(config["mcpServers"]["docs"]["command"], "x");
    assert_eq!(config["projects"]["/work"]["hasTrustDialogAccepted"], true);
    assert!(config.get("primaryApiKey").is_none());
    assert_eq!(
        std::fs::read_link(profile.join("settings.json")).unwrap(),
        world.home.join(".claude/settings.json")
    );
    assert!(
        std::fs::symlink_metadata(profile.join("skills"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    // The default login was not touched.
    assert_eq!(world.live_access(), "at:B:1");

    // The account that is already the default login runs without a profile.
    let ran = world.ok(&["run", "2"]);
    assert!(ran.out.contains("config_dir=default"), "{}", ran.out);

    // A token the session refreshed inside the profile comes back to the store.
    write(
        &profile.join(".credentials.json"),
        &credential("A", 4, now_ms() + 9 * HOUR_MS),
    );
    world.list(&["--fresh"]);
    assert_eq!(world.secret(1)["claudeAiOauth"]["refreshToken"], "rt:A:4");
}

#[test]
fn bare_run_picks_the_account_with_most_room() {
    let world = World::with_two_accounts();
    world.install_fake_claude();
    world.set_usage("A", 10.0, 10.0);
    world.set_usage("B", 70.0, 10.0);
    let ran = world.ok(&["run"]);
    assert!(ran.out.contains("account=1"), "{}", ran.out);
    // Everything at its limit: refuse, and say when one frees up.
    world.set_usage("A", 100.0, 10.0);
    world.set_usage("B", 100.0, 10.0);
    world.ok(&["ls", "--fresh"]);
    let refused = world.cx(&["run"]);
    assert_eq!(refused.code, 1);
    assert!(refused.err.contains("every account is at its limit"), "{}", refused.err);
}

#[test]
fn api_keys_are_only_used_when_asked_for() {
    let world = World::with_two_accounts();
    world.install_fake_claude();
    let added = world.cx_with(&["add-key", "--label", "console"], &[]);
    // No terminal: the key is read from stdin, which is empty here.
    assert_eq!(added.code, 1);
    let mut child = Command::new(env!("CARGO_BIN_EXE_cx"))
        .args(["add-key", "--label", "console"])
        .env_clear()
        .env("HOME", &world.home)
        .env("CX_HOME", world.home.join(".cx"))
        .env("CX_SECRETS", "file")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"sk-ant-api03-test-key\n")
        .unwrap();
    assert!(child.wait().unwrap().success());

    let list = world.list(&[]);
    assert_eq!(account(&list, 3)["kind"], "api-key");
    assert_eq!(account(&list, 3)["eligible"], false);
    assert!(world.cx(&["use", "3"]).err.contains("cx run 3"));
    let ran = world.ok(&["run", "console"]);
    assert!(
        ran.out.contains("api_key=sk-ant-api03-test-key") && ran.out.contains("config_dir=default"),
        "{}",
        ran.out
    );

    // With every subscription exhausted the key is still not used...
    world.set_usage("A", 100.0, 10.0);
    world.set_usage("B", 100.0, 10.0);
    world.ok(&["ls", "--fresh"]);
    assert_eq!(world.cx(&["run"]).code, 1);
    // ...until the fallback is switched on.
    world.ok(&["config", "api_fallback", "true"]);
    let ran = world.ok(&["run"]);
    assert!(ran.out.contains("api_key=sk-ant-api03-test-key"), "{}", ran.out);
    assert!(ran.err.contains("billed per token"), "{}", ran.err);
}

#[test]
fn login_signs_in_through_claude_and_cleans_up() {
    let world = World::with_two_accounts();
    world.install_fake_claude();
    let ran = world.ok(&["login"]);
    assert!(ran.out.contains("added #3 C@example.test"), "{}", ran.out);
    assert_eq!(world.secret(3)["claudeAiOauth"]["refreshToken"], "rt:C:1");
    // The throwaway config directory is gone and the default login untouched.
    assert_eq!(std::fs::read_dir(world.home.join(".cx/login")).unwrap().count(), 0);
    assert_eq!(world.live_access(), "at:B:1");
    // Signing in again to the same account updates it in place.
    let again = world.ok(&["login", "3"]);
    assert!(again.out.contains("signed in again to #3"), "{}", again.out);
    // Asking for one account and signing in as another is reported.
    let wrong = world.ok(&["login", "1"]);
    assert!(
        wrong
            .err
            .contains("you signed in as C@example.test, not A@example.test"),
        "{}",
        wrong.err
    );
    assert_eq!(world.list(&[])["accounts"].as_array().unwrap().len(), 3);
}

#[test]
fn an_unmanaged_default_login_is_put_aside_not_overwritten() {
    let world = World::with_two_accounts();
    world.sign_in_claude("Z", 1, now_ms() + 8 * HOUR_MS);
    let list = world.list(&[]);
    assert_eq!(list["activeAccount"], Value::Null);
    assert_eq!(list["defaultLoginUnmanaged"], true);
    let ran = world.ok(&["use", "1"]);
    assert!(ran.err.contains("was kept"), "{}", ran.err);
    let stash = std::fs::read_dir(world.home.join(".cx/secrets"))
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("stash-"))
        .expect("the unmanaged login should be stashed");
    assert_eq!(read(&stash.path())["claudeAiOauth"]["refreshToken"], "rt:Z:1");
    assert_eq!(world.live_access(), "at:A:1");
}

#[test]
fn import_from_claude_swap_copies_accounts_and_never_goes_backwards() {
    let world = World::new();
    let root = world.home.join(".claude-swap-backup");
    write(
        &root.join("sequence.json"),
        &json!({"activeAccountNumber": 1, "sequence": [1, 3], "accounts": {
            "1": {"email": "A@example.test", "uuid": "uuid-A", "organizationUuid": "org-A", "organizationName": "A org"},
            "3": {"email": "B@example.test", "uuid": "uuid-B", "organizationUuid": "org-B", "organizationName": "B org", "disabled": true, "alias": "spare"},
            "4": {"email": "gone@example.test", "uuid": "uuid-G", "organizationUuid": "org-G"}
        }}),
    );
    let encode = |value: &Value| {
        let text = value.to_string();
        let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in text.as_bytes().chunks(3) {
            let n = (chunk[0] as u32) << 16
                | (*chunk.get(1).unwrap_or(&0) as u32) << 8
                | *chunk.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                out.push(if i <= chunk.len() {
                    table[((n >> (18 - 6 * i)) & 63) as usize] as char
                } else {
                    '='
                });
            }
        }
        out
    };
    std::fs::create_dir_all(root.join("credentials")).unwrap();
    std::fs::write(
        root.join("credentials/.creds-1-A@example.test.enc"),
        encode(&credential("A", 2, now_ms() + HOUR_MS)),
    )
    .unwrap();
    std::fs::write(
        root.join("credentials/.creds-3-B@example.test.enc"),
        encode(&credential("B", 2, now_ms() - HOUR_MS)),
    )
    .unwrap();
    write(
        &root.join("configs/.claude-config-1-A@example.test.json"),
        &json!({"oauthAccount": oauth_account("A"), "theme": "x"}),
    );

    let dry = world.ok(&["import-cswap", "--dry-run"]);
    assert!(
        dry.out.contains("would import") && dry.out.contains("no stored credential found"),
        "{}",
        dry.out
    );
    assert!(!world.home.join(".cx/secrets").exists());

    world.ok(&["import-cswap"]);
    let list = world.list(&["--offline"]);
    assert_eq!(account(&list, 1)["label"], "A@example.test");
    assert_eq!(account(&list, 3)["alias"], "spare");
    assert_eq!(account(&list, 3)["disabled"], true);
    assert_eq!(list["accounts"].as_array().unwrap().len(), 2);
    assert_eq!(world.secret(1)["claudeAiOauth"]["refreshToken"], "rt:A:2");

    // cx then refreshes A. Importing again must not put the old token back.
    world.ok(&["refresh", "1"]);
    assert_eq!(world.secret(1)["claudeAiOauth"]["refreshToken"], "rt:A:3");
    let again = world.ok(&["import-cswap"]);
    assert!(again.out.contains("already in cx as #1"), "{}", again.out);
    assert_eq!(world.secret(1)["claudeAiOauth"]["refreshToken"], "rt:A:3");
    // claude-swap's own files are untouched.
    assert!(root.join("credentials/.creds-1-A@example.test.enc").exists());

    // The imported account works as the default login, with its config section.
    world.ok(&["use", "1"]);
    assert_eq!(read(&world.config_file())["oauthAccount"]["displayName"], "A");
}

#[test]
fn a_token_reply_cx_cannot_read_is_kept_not_dropped() {
    let world = World::with_two_accounts();
    world.set_secret(1, &credential("A", 1, now_ms() - HOUR_MS));
    world.server.lock().unwrap().garbled_token_replies = true;
    let ran = world.cx(&["refresh", "1"]);
    assert_eq!(ran.code, 1);
    assert!(ran.err.contains("so the new token is not lost"), "{}", ran.err);
    let kept = std::fs::read_dir(world.home.join(".cx/secrets"))
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("acct-1.reply-"))
        .expect("the reply should be kept");
    assert!(std::fs::read_to_string(kept.path()).unwrap().contains("rt:A:2"));
    // The account is not written off: this was not a rejection.
    assert_eq!(account(&world.list(&["--offline"]), 1)["needsLogin"], false);
}

#[test]
fn account_housekeeping() {
    let world = World::with_two_accounts();
    world.ok(&["alias", "1", "work"]);
    assert!(
        world
            .cx(&["alias", "2", "work"])
            .err
            .contains("already another account's alias")
    );
    world.ok(&["use", "work"]);
    assert_eq!(world.live_access(), "at:A:1");
    world.ok(&["priority", "2", "5"]);
    assert_eq!(account(&world.list(&[]), 2)["priority"], 5);

    assert!(
        world
            .cx(&["config", "threshold_5h", "20"])
            .err
            .contains("between 50 and 100")
    );
    world.ok(&["config", "threshold_5h", "80"]);
    world.ok(&["config", "strategy", "drain-soonest"]);
    world.ok(&["config", "model", "Fable"]);
    assert!(world.cx(&["config", "strategy", "nonsense"]).code != 0);
    let shown = world.ok(&["config"]).out;
    assert!(
        shown.contains("threshold_5h   80") && shown.contains("drain-soonest") && shown.contains("Fable"),
        "{shown}"
    );

    // Removal needs a yes when there is no terminal to ask.
    assert!(world.cx(&["rm", "2"]).err.contains("--yes"));
    world.ok(&["rm", "2", "--yes"]);
    assert_eq!(world.list(&[])["accounts"].as_array().unwrap().len(), 1);
    assert!(!world.home.join(".cx/secrets/acct-2.secret").exists());
}

#[test]
fn two_processes_refreshing_one_account_spend_the_token_once() {
    let world = World::with_two_accounts();
    world.set_secret(1, &credential("A", 1, now_ms() - HOUR_MS));
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| world.cx(&["ls", "--fresh", "--json"])))
            .collect();
        for handle in handles {
            let ran = handle.join().unwrap();
            assert_eq!(ran.code, 0, "{}", ran.err);
        }
    });
    // Four processes raced; the refresh token was exchanged exactly once.
    assert_eq!(world.server.lock().unwrap().refreshes, 1);
    assert_eq!(world.secret(1)["claudeAiOauth"]["refreshToken"], "rt:A:2");
    assert_eq!(account(&world.list(&[]), 1)["needsLogin"], false);
}

#[test]
fn a_pinned_directory_decides_where_a_bare_run_goes() {
    let world = World::with_two_accounts();
    world.install_fake_claude();
    // #2 has far more room, so without a pin a bare run goes there.
    world.set_usage("A", 70.0, 70.0);
    world.set_usage("B", 5.0, 5.0);
    let project = world.home.join("work/client");
    let inside = project.join("api/src");
    std::fs::create_dir_all(&inside).unwrap();
    assert!(world.cx_in(&inside, &["run"]).out.contains("account=2"));

    let pinned = world.cx_in(&project, &["map", "1"]);
    assert_eq!(pinned.code, 0, "{}", pinned.err);
    let ran = world.cx_in(&inside, &["run"]);
    assert!(ran.out.contains("account=1"), "{}", ran.out);
    assert!(ran.err.contains("is pinned to it"), "{}", ran.err);
    // Outside the pinned tree the choice is by room again.
    assert!(world.cx_in(&world.home, &["run"]).out.contains("account=2"));
    assert!(world.ok(&["map"]).out.contains("work/client"));

    // A named account still wins over the pin, and removing the account drops its pins.
    assert!(world.cx_in(&inside, &["run", "2"]).out.contains("account=2"));
    world.ok(&["rm", "1", "--yes"]);
    assert!(world.ok(&["map"]).out.contains("No directory is pinned"));

    let again = world.cx_in(&project, &["map", "2"]);
    assert_eq!(again.code, 0, "{}", again.err);
    assert_eq!(world.cx_in(&inside, &["unmap", project.to_str().unwrap()]).code, 0);
    assert!(world.ok(&["map"]).out.contains("No directory is pinned"));
}

#[test]
fn an_export_restores_every_account_on_another_machine() {
    let first = World::with_two_accounts();
    first.ok(&["alias", "1", "main"]);
    first.ok(&["config", "threshold_5h", "80"]);
    let file = first.home.join("backup.cx");
    let path = file.to_str().unwrap();

    let short = first.cx_with(&["export", path], &[("CX_PASSPHRASE", "short")]);
    assert_ne!(short.code, 0);
    assert!(short.err.contains("at least 8 characters"), "{}", short.err);
    let made = first.cx_with(&["export", path], &[("CX_PASSPHRASE", "correct horse battery")]);
    assert_eq!(made.code, 0, "{}", made.err);
    let sealed = std::fs::read_to_string(&file).unwrap();
    assert!(sealed.starts_with("-----BEGIN AGE ENCRYPTED FILE-----"));
    assert!(!sealed.contains("at:A") && !sealed.contains("example.test"));
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    // An existing file is not replaced without --force.
    assert_ne!(
        first
            .cx_with(&["export", path], &[("CX_PASSPHRASE", "correct horse battery")])
            .code,
        0
    );

    let second = World::new();
    let wrong = second.cx_with(&["import", path], &[("CX_PASSPHRASE", "not the passphrase")]);
    assert_ne!(wrong.code, 0);
    assert!(wrong.err.contains("wrong passphrase"), "{}", wrong.err);
    assert_eq!(second.list(&["--offline"])["accounts"].as_array().unwrap().len(), 0);

    let restored = second.cx_with(&["import", path], &[("CX_PASSPHRASE", "correct horse battery")]);
    assert_eq!(restored.code, 0, "{}", restored.err);
    let list = second.list(&["--offline"]);
    assert_eq!(account(&list, 1)["alias"], "main");
    assert_eq!(account(&list, 2)["label"], "B@example.test");
    assert_eq!(second.secret(1)["claudeAiOauth"]["refreshToken"], "rt:A:1");
    assert_eq!(second.secret(2)["claudeAiOauth"]["accessToken"], "at:B:1");
    // Settings stay as they were unless asked for.
    assert_eq!(second.ok(&["config", "threshold_5h"]).out.trim(), "90");

    // A second import changes nothing; --settings brings the settings.
    let again = second.cx_with(
        &["import", path, "--settings"],
        &[("CX_PASSPHRASE", "correct horse battery")],
    );
    assert!(again.out.contains("already here as #1"), "{}", again.out);
    assert_eq!(second.ok(&["config", "threshold_5h"]).out.trim(), "80");
    assert_eq!(second.list(&["--offline"])["accounts"].as_array().unwrap().len(), 2);
    // The restored login works: the server accepts its token.
    second.set_usage("A", 33.0, 44.0);
    assert_eq!(account(&second.list(&[]), 1)["usage"]["five_hour"]["pct"], 33.0);
}

#[test]
fn the_status_line_names_the_session_account_without_the_network() {
    let world = World::with_two_accounts();
    world.set_usage("A", 30.0, 50.0);
    world.set_usage("B", 92.0, 10.0);
    world.ok(&["ls"]);
    let reads = world.server.lock().unwrap().usage_reads;
    // No CX_ACCOUNT: the default login, which is B.
    assert_eq!(world.ok(&["statusline"]).out.trim(), "#2 B · 5h 92% · wk 10%");
    let inside = world.cx_with(&["statusline"], &[("CX_ACCOUNT", "1")]);
    assert_eq!(inside.out.trim(), "#1 A · 5h 30% · wk 50%");
    assert_eq!(world.server.lock().unwrap().usage_reads, reads);
    world.ok(&["alias", "1", "main"]);
    assert!(
        world
            .cx_with(&["statusline"], &[("CX_ACCOUNT", "1")])
            .out
            .starts_with("#1 main")
    );
}

#[test]
fn usage_readings_are_kept_as_history_when_they_move() {
    let world = World::with_two_accounts();
    let lines = || {
        std::fs::read_to_string(world.home.join(".cx/history.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .collect::<Vec<_>>()
    };
    world.set_usage("A", 10.0, 20.0);
    world.set_usage("B", 1.0, 2.0);
    world.ok(&["ls", "--fresh"]);
    assert_eq!(lines().len(), 2);
    // The same numbers again add nothing.
    world.ok(&["ls", "--fresh"]);
    assert_eq!(lines().len(), 2);
    world.set_usage("A", 14.0, 20.0);
    world.ok(&["ls", "--fresh"]);
    let all = lines();
    assert_eq!(all.len(), 3);
    assert_eq!((all[2]["a"].as_u64(), all[2]["h5"].as_f64()), (Some(1), Some(14.0)));
}

#[test]
fn settings_are_listed_with_help_and_checked_when_set() {
    let world = World::new();
    let listed = world.ok(&["config"]).out;
    assert!(
        listed.contains("notify") && listed.contains("Desktop notifications"),
        "{listed}"
    );
    world.ok(&["config", "notify", "off"]);
    assert_eq!(world.ok(&["config", "notify"]).out.trim(), "false");
    for bad in [
        ["config", "notify", "sometimes"],
        ["config", "threshold_7d", "10"],
        ["config", "interval_s", "2"],
        ["config", "nonsense", "1"],
    ] {
        assert_ne!(world.cx(&bad).code, 0, "{bad:?} was accepted");
    }
    let script = world.ok(&["completions", "zsh"]).out;
    assert!(script.starts_with("#compdef cx") && script.contains("import-cswap"));
}

#[test]
fn auto_notifies_when_it_switches() {
    let world = World::with_two_accounts();
    world.ok(&["use", "1"]);
    world.set_usage("A", 96.0, 20.0);
    world.set_usage("B", 10.0, 20.0);
    let mut watcher = world
        .command(&["auto"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let log = world.home.join("notify.log");
    let started = std::time::Instant::now();
    while !log.exists() && started.elapsed() < std::time::Duration::from_secs(20) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = watcher.kill();
    let _ = watcher.wait();
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(text.starts_with("cx: Now on #2 B"), "{text:?}");
    assert_eq!(world.live_access(), "at:B:1");
}
