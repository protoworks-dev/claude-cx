//! `cx hook`: what Claude Code tells cx while a session runs.
//!
//! Claude Code can run a command at points in a session (a session starts,
//! a prompt is sent, a tool fails, the conversation is summarised) and hands
//! it a JSON description on standard input. The cx plugin points those
//! hooks here. Each one becomes a single short line in `events.jsonl` in
//! the cx state directory: the event, the session, the project and a few
//! plain facts such as the tool's name. Prompt text, tool input and tool
//! output are never written.
//!
//! One event does more than record. When a prompt is sent after the
//! conversation's cache has expired, the whole conversation is read again
//! at full price; cx says so, with the size, so the user can decide to
//! clear or summarise next time.
//!
//! A hook must never break the session it runs in, so every failure here is
//! swallowed and the command always succeeds.

use crate::util;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// The events file is cut back to its newer half when it passes this size.
const MAX_BYTES: u64 = 4 << 20;
/// A conversation smaller than this is not worth a notice when its cache
/// has expired.
const NOTICE_TOKENS: u64 = 100_000;
/// How long Claude Code keeps a main conversation's cache on a
/// subscription, and with an API key, in milliseconds.
const SUBSCRIPTION_TTL_MS: i64 = 60 * 60_000;
const KEY_TTL_MS: i64 = 5 * 60_000;

/// One recorded hook event. The short serde names keep the file small.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// When cx received it, in milliseconds since the Unix epoch.
    #[serde(rename = "t")]
    pub at_ms: i64,
    /// The hook event's name, such as `SessionStart`.
    #[serde(rename = "e")]
    pub event: String,
    /// The Claude Code session id.
    #[serde(rename = "s", default, skip_serializing_if = "String::is_empty")]
    pub session: String,
    /// The last two parts of the session's working directory.
    #[serde(rename = "p", default, skip_serializing_if = "String::is_empty")]
    pub project: String,
    /// The tool's name, for tool events.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool: String,
    /// The subagent's type, for subagent events.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    /// The event's own qualifier: how the session started, why it ended,
    /// what triggered a summary, what kind of notification it was.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// The model, where the event names one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// A tool failure that was the user stopping the tool.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupt: bool,
    /// How long the tool ran, in milliseconds.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ms: u64,
    /// The prompt's length in characters, for a prompt. Its text is not kept.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub chars: u64,
    /// For a prompt sent on an expired cache: the tokens read again.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cold_tokens: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn text(input: &Value, key: &str) -> String {
    input.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn project_of(cwd: &str) -> String {
    let parts: Vec<&str> = cwd.split('/').filter(|p| !p.is_empty()).collect();
    match parts.len() {
        0 => String::new(),
        1 => parts[0].into(),
        n => format!("{}/{}", parts[n - 2], parts[n - 1]),
    }
}

/// Turn a hook's input into the line cx keeps. Only names, kinds and
/// numbers are taken from it.
pub fn event_of(input: &Value, now_ms: i64) -> Event {
    let model = match input.get("model") {
        Some(Value::String(id)) => id.clone(),
        Some(model) => text(model, "id"),
        None => String::new(),
    };
    let detail = ["source", "reason", "trigger", "notification_type"]
        .iter()
        .map(|key| text(input, key))
        .find(|value| !value.is_empty())
        .unwrap_or_default();
    Event {
        at_ms: now_ms,
        event: text(input, "hook_event_name"),
        session: text(input, "session_id"),
        project: project_of(&text(input, "cwd")),
        tool: text(input, "tool_name"),
        agent: text(input, "agent_type"),
        detail,
        model,
        interrupt: input.get("is_interrupt").and_then(Value::as_bool).unwrap_or(false),
        ms: input.get("duration_ms").and_then(Value::as_f64).unwrap_or(0.0).max(0.0) as u64,
        chars: input
            .get("prompt")
            .and_then(Value::as_str)
            .map_or(0, |p| p.chars().count() as u64),
        cold_tokens: 0,
    }
}

/// The time of the newest reply in a transcript and the size of the
/// conversation it answered, read from the file's tail.
fn last_reply(transcript: &Path) -> Option<(i64, u64)> {
    let mut file = std::fs::File::open(transcript).ok()?;
    let size = file.metadata().ok()?.len();
    const TAIL: u64 = 2 << 20;
    file.seek(SeekFrom::Start(size.saturating_sub(TAIL))).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    bytes
        .split(|b| *b == b'\n')
        .rev()
        .filter(|line| memchr::memmem::find(line, b"\"type\":\"assistant\"").is_some())
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find_map(|reply| {
            let usage = reply.get("message")?.get("usage")?;
            let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
            let tokens =
                count("input_tokens") + count("cache_read_input_tokens") + count("cache_creation_input_tokens");
            let at = util::parse_rfc3339_ms(reply.get("timestamp")?.as_str()?)?;
            (tokens > 0).then_some((at, tokens))
        })
}

fn tokens_short(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1e6)
    } else {
        format!("{}k", tokens / 1000)
    }
}

/// For a prompt: whether the conversation's cache has expired, and if so
/// how many tokens the prompt makes Claude Code read again and how long
/// the conversation sat. `with_key` says the session is billed to an API
/// key, whose cache lasts five minutes and not an hour.
pub fn cold_prompt(transcript: &Path, now_ms: i64, with_key: bool) -> Option<(u64, i64)> {
    let (at, tokens) = last_reply(transcript)?;
    let idle = now_ms - at;
    let ttl = if with_key { KEY_TTL_MS } else { SUBSCRIPTION_TTL_MS };
    (idle >= ttl && tokens >= NOTICE_TOKENS).then_some((tokens, idle))
}

fn events_path(store_root: &Path) -> PathBuf {
    store_root.join("events.jsonl")
}

fn append(store_root: &Path, event: &Event) -> std::io::Result<()> {
    let path = events_path(store_root);
    if event.event == "SessionStart"
        && let Ok(meta) = std::fs::metadata(&path)
        && meta.len() > MAX_BYTES
        && let Ok(bytes) = std::fs::read(&path)
    {
        // Keep the newer half, from the start of a line.
        let half = bytes.len() / 2;
        let from = bytes[half..]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(half, |i| half + i + 1);
        let _ = util::atomic_write(&path, &bytes[from..], 0o600);
    }
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // One write call for the whole line, so two sessions cannot interleave.
    options.open(path)?.write_all(&line)
}

/// Record one hook call and return what to print for Claude Code, if
/// anything. `input` is the JSON Claude Code gave the hook.
pub fn handle(store_root: &Path, input: &Value, now_ms: i64, with_key: bool) -> Option<Value> {
    let mut event = event_of(input, now_ms);
    if event.event.is_empty() {
        return None;
    }
    let mut reply = None;
    if event.event == "UserPromptSubmit"
        && let Some(transcript) = input.get("transcript_path").and_then(Value::as_str)
        && let Some((tokens, idle)) = cold_prompt(Path::new(transcript), now_ms, with_key)
    {
        event.cold_tokens = tokens;
        reply = Some(json!({
            "systemMessage": format!(
                "cx: this conversation sat for {}, so its cache has expired. This prompt re-reads about {} tokens at full price. For a new task, /clear first; to keep the thread, /compact makes it smaller.",
                util::human_duration(idle / 1000),
                tokens_short(tokens),
            )
        }));
    }
    let _ = util::mkdir_private(store_root);
    let _ = append(store_root, &event);
    reply
}

/// Read a hook's input from standard input, record it, and print the
/// reply. Never fails: a hook that errors would disturb the session.
pub fn run(store_root: &Path) {
    let mut bytes = Vec::new();
    if std::io::stdin().take(8 << 20).read_to_end(&mut bytes).is_err() {
        return;
    }
    let Ok(input) = serde_json::from_slice::<Value>(&bytes) else {
        return;
    };
    let with_key = std::env::var_os("ANTHROPIC_API_KEY").is_some_and(|k| !k.is_empty());
    if let Some(reply) = handle(store_root, &input, util::now_ms(), with_key) {
        println!("{reply}");
    }
}

/// Every recorded event from `since_ms` on, oldest first.
pub fn load(store_root: &Path, since_ms: i64) -> Vec<Event> {
    let Ok(text) = std::fs::read_to_string(events_path(store_root)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Event>(line).ok())
        .filter(|event| event.at_ms >= since_ms)
        .collect()
}

/// What the recorded events add up to, for the report.
pub fn summary(events: &[Event], now_ms: i64) -> Value {
    let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
    let mut failures: BTreeMap<&str, u64> = BTreeMap::new();
    let mut denied: BTreeMap<&str, u64> = BTreeMap::new();
    let mut agents: BTreeMap<&str, u64> = BTreeMap::new();
    let mut starts: BTreeMap<&str, u64> = BTreeMap::new();
    let mut open: BTreeMap<&str, &Event> = BTreeMap::new();
    let mut sessions: BTreeSet<&str> = BTreeSet::new();
    let (mut cold, mut cold_tokens, mut stopped_tools) = (0u64, 0u64, 0u64);
    for event in events {
        *counts.entry(event.event.as_str()).or_default() += 1;
        if !event.session.is_empty() {
            sessions.insert(&event.session);
        }
        match event.event.as_str() {
            "SessionStart" => {
                *starts.entry(event.detail.as_str()).or_default() += 1;
                open.insert(&event.session, event);
            }
            "SessionEnd" => {
                open.remove(event.session.as_str());
            }
            "PostToolUseFailure" if event.interrupt => stopped_tools += 1,
            "PostToolUseFailure" => *failures.entry(event.tool.as_str()).or_default() += 1,
            "PermissionDenied" => *denied.entry(event.tool.as_str()).or_default() += 1,
            "SubagentStart" => *agents.entry(event.agent.as_str()).or_default() += 1,
            "UserPromptSubmit" if event.cold_tokens > 0 => {
                cold += 1;
                cold_tokens += event.cold_tokens;
            }
            _ => {}
        }
    }
    let ranked = |map: BTreeMap<&str, u64>| {
        let mut rows: Vec<(&str, u64)> = map.into_iter().collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.1));
        rows.into_iter()
            .map(|(name, count)| json!({"name": name, "count": count}))
            .collect::<Vec<_>>()
    };
    // A session that started more than a day ago and never ended was
    // most likely killed without its end hook running.
    let open_now: Vec<Value> = open
        .values()
        .filter(|event| now_ms - event.at_ms < 86_400_000)
        .map(|event| json!({"project": event.project, "model": event.model, "started_minutes_ago": (now_ms - event.at_ms) / 60_000}))
        .collect();
    json!({
        "recorded": !events.is_empty(),
        "sessions_seen": sessions.len(),
        "sessions_open_now": open_now,
        "session_starts_by_kind": ranked(starts),
        "events": counts,
        "prompts_sent_on_an_expired_cache": {"count": cold, "tokens_read_again": cold_tokens},
        "tools_the_user_stopped": stopped_tools,
        "tool_failures": ranked(failures),
        "permission_denials": ranked(denied),
        "subagents_started": ranked(agents),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cx-hook-{}-{}", std::process::id(), fastrand::u32(..)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_event_keeps_names_and_numbers_and_no_text() {
        let root = root();
        let input = json!({
            "hook_event_name": "PostToolUseFailure", "session_id": "s1", "cwd": "/home/me/work/app",
            "tool_name": "Bash", "tool_input": {"command": "rm -rf secret-dir"}, "error": "secret failure",
            "is_interrupt": true, "duration_ms": 1200,
        });
        assert_eq!(handle(&root, &input, 1000, false), None);
        let prompt = json!({"hook_event_name": "UserPromptSubmit", "session_id": "s1", "prompt": "my secret plan"});
        handle(&root, &prompt, 2000, false);
        let saved = std::fs::read_to_string(events_path(&root)).unwrap();
        assert!(!saved.contains("secret"), "{saved}");
        let events = load(&root, 0);
        assert_eq!(events.len(), 2);
        assert_eq!(
            (
                events[0].tool.as_str(),
                events[0].project.as_str(),
                events[0].interrupt,
                events[0].ms
            ),
            ("Bash", "work/app", true, 1200)
        );
        assert_eq!(events[1].chars, 14);
        assert_eq!(load(&root, 1500).len(), 1);
    }

    #[test]
    fn a_prompt_on_an_expired_cache_gets_a_notice() {
        let root = root();
        let transcript = root.join("t.jsonl");
        let now = util::now_ms();
        let reply = |minutes_ago: i64, cached: u64| {
            json!({"type": "assistant",
                "timestamp": (chrono::Utc::now() - chrono::Duration::minutes(minutes_ago)).to_rfc3339(),
                "message": {"usage": {"input_tokens": 10, "cache_read_input_tokens": cached}}})
            .to_string()
                + "\n"
        };
        let prompt = json!({"hook_event_name": "UserPromptSubmit", "session_id": "s", "prompt": "go",
            "transcript_path": transcript.to_string_lossy()});

        std::fs::write(&transcript, reply(90, 300_000)).unwrap();
        let notice = handle(&root, &prompt, now, false).expect("a notice");
        let message = notice["systemMessage"].as_str().unwrap();
        assert!(
            message.contains("300k tokens") && message.contains("/clear"),
            "{message}"
        );
        assert_eq!(load(&root, 0)[0].cold_tokens, 300_010);

        // Within the hour on a subscription the cache is still warm...
        std::fs::write(&transcript, reply(20, 300_000)).unwrap();
        assert_eq!(handle(&root, &prompt, now, false), None);
        // ...but with an API key it lasts five minutes.
        assert!(handle(&root, &prompt, now, true).is_some());
        // A small conversation is not worth a notice.
        std::fs::write(&transcript, reply(90, 20_000)).unwrap();
        assert_eq!(handle(&root, &prompt, now, false), None);
    }

    #[test]
    fn the_summary_counts_open_sessions_and_failures() {
        let event = |at_ms, event: &str, session: &str| Event {
            at_ms,
            event: event.into(),
            session: session.into(),
            ..Event::default()
        };
        let now = 10 * 86_400_000;
        let mut failed = event(now - 500, "PostToolUseFailure", "a");
        failed.tool = "Bash".into();
        let mut stopped = failed.clone();
        stopped.interrupt = true;
        let events = vec![
            event(now - 3 * 86_400_000, "SessionStart", "old"),
            event(now - 4000, "SessionStart", "a"),
            event(now - 3000, "SessionStart", "b"),
            event(now - 2000, "SessionEnd", "b"),
            failed,
            stopped,
        ];
        let summary = summary(&events, now);
        assert_eq!(summary["sessions_seen"], 3);
        assert_eq!(summary["sessions_open_now"].as_array().unwrap().len(), 1);
        assert_eq!(summary["tools_the_user_stopped"], 1);
        assert_eq!(summary["tool_failures"][0], json!({"name": "Bash", "count": 1}));
    }
}
