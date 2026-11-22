//! What each account was used for, read from Claude Code's own transcripts
//! under `~/.claude/projects`.
//!
//! A transcript names the account it ran on (`session-owner` lines) and, for
//! every reply, the model, the working directory and the token counts,
//! including what was written to and read from the prompt cache. Subagent
//! runs sit in `<session>/subagents/` and belong to the session's account.
//! Hook runs are recorded with their duration.
//!
//! The transcripts are large (gigabytes), so a file is read once and then
//! only from where the last read stopped. The sums per file are kept in
//! `activity.json` in the cx state directory. Nothing is ever written to
//! `~/.claude`, and no message text is kept: a prompt's first words are
//! looked at to count corrections, and only the count is stored.

use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Bumped when what is kept per file changes, so old sums are read again.
const CACHE_VERSION: u32 = 5;
/// Days of activity kept and shown.
pub const DAYS: i64 = 7;
/// A reply that wrote this many tokens to the cache while reading fewer
/// than it wrote had to rebuild the conversation's cache.
const REBUILD_TOKENS: u64 = 20_000;
/// A file edited this many times in one conversation is listed as churn.
pub const CHURN_EDITS: u32 = 8;
/// The prompt cache is kept for five minutes by default, and for an hour
/// where the longer lifetime is in use.
const SHORT_IDLE_MS: i64 = 5 * 60_000;
const LONG_IDLE_MS: i64 = 60 * 60_000;

/// Why a reply had to write the conversation to the prompt cache again.
pub mod cause {
    /// The first reply of a conversation or of a subagent run: there was
    /// nothing in the cache yet. This one cannot be avoided.
    pub const START: &str = "start";
    /// More than an hour passed since the previous reply.
    pub const LONG_IDLE: &str = "idle over an hour";
    /// Between five minutes and an hour passed since the previous reply.
    pub const IDLE: &str = "idle over 5 minutes";
    /// The conversation had just been summarised.
    pub const COMPACT: &str = "after a summary";
    /// The model changed from the previous reply.
    pub const MODEL: &str = "model changed";
    /// None of the above: the tools, the system prompt or a memory file
    /// changed, or the cache was dropped for a reason the transcript does
    /// not record.
    pub const OTHER: &str = "something else changed";
    /// Added to a cause when the rebuild happened inside a subagent run.
    pub const IN_SUBAGENT: &str = ", in a subagent";
}

/// Token counts summed over a set of replies, as the transcripts report
/// them. The short serde names keep `activity.json` small.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    /// Input tokens that were neither read from nor written to the prompt
    /// cache.
    #[serde(default, rename = "i")]
    pub input: u64,
    /// Tokens the model wrote in its replies.
    #[serde(default, rename = "o")]
    pub output: u64,
    /// Input tokens written to the prompt cache.
    #[serde(default, rename = "w")]
    pub cache_write: u64,
    /// Input tokens served from the prompt cache.
    #[serde(default, rename = "r")]
    pub cache_read: u64,
    /// Number of API requests. A reply written as several transcript lines
    /// counts once.
    #[serde(default, rename = "n")]
    pub requests: u64,
    /// Replies that rebuilt the cache, and the tokens they wrote doing it.
    #[serde(default, rename = "b")]
    pub rebuilds: u64,
    /// Tokens written to the cache by the replies counted in `rebuilds`.
    /// These are also included in `cache_write`.
    #[serde(default, rename = "bt")]
    pub rebuilt: u64,
}

impl Tokens {
    /// Add every count of `other` to this one.
    pub fn add(&mut self, other: &Tokens) {
        self.input += other.input;
        self.output += other.output;
        self.cache_write += other.cache_write;
        self.cache_read += other.cache_read;
        self.requests += other.requests;
        self.rebuilds += other.rebuilds;
        self.rebuilt += other.rebuilt;
    }

    /// One number for "how much": tokens weighted the way the API prices
    /// them (output 5, cache write 1.25, input 1, cache read 0.1). The
    /// subscription limits are not published, so this is a yardstick for
    /// comparing rows, not the server's own count.
    pub fn weight(&self) -> f64 {
        self.input as f64 + self.output as f64 * 5.0 + self.cache_write as f64 * 1.25 + self.cache_read as f64 * 0.1
    }
}

/// Work that was paid for and then thrown away or repeated: turns the user
/// stopped, prompts that corrected the previous turn, conversations that
/// were summarised, and caches that were written again.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Waste {
    /// Prompts the user typed (commands and tool results are not counted).
    #[serde(default, rename = "p")]
    pub prompts: u64,
    /// Times the user pressed Escape while Claude was writing or thinking.
    #[serde(default, rename = "s")]
    pub stops: u64,
    /// Times the user stopped or refused a tool call.
    #[serde(default, rename = "ts")]
    pub tool_stops: u64,
    /// Tokens Claude had written in the turns that were stopped, from the
    /// user's prompt up to the stop.
    #[serde(default, rename = "lo")]
    pub lost_output: u64,
    /// Prompts typed straight after a stop: the user steering again.
    #[serde(default, rename = "rd")]
    pub redirects: u64,
    /// Other prompts that open by saying the last turn was wrong ("no,",
    /// "I said", "still not"). Counted by their wording, so an estimate.
    #[serde(default, rename = "c")]
    pub corrections: u64,
    /// Summaries Claude Code started itself because the conversation was full.
    #[serde(default, rename = "ac")]
    pub auto_summaries: u64,
    /// Summaries the user asked for.
    #[serde(default, rename = "mc")]
    pub asked_summaries: u64,
    /// Tokens in the conversations at the moment they were summarised.
    #[serde(default, rename = "ct")]
    pub summarised: u64,
    /// Time spent writing those summaries, in milliseconds.
    #[serde(default, rename = "cm")]
    pub summary_ms: u64,
    /// Cache rebuilds by [`cause`]: how many, and the tokens written.
    #[serde(default, rename = "rb", skip_serializing_if = "BTreeMap::is_empty")]
    pub rebuilds: BTreeMap<String, (u64, u64)>,
}

impl Waste {
    /// Add every count of `other` to this one.
    pub fn add(&mut self, other: &Waste) {
        self.prompts += other.prompts;
        self.stops += other.stops;
        self.tool_stops += other.tool_stops;
        self.lost_output += other.lost_output;
        self.redirects += other.redirects;
        self.corrections += other.corrections;
        self.auto_summaries += other.auto_summaries;
        self.asked_summaries += other.asked_summaries;
        self.summarised += other.summarised;
        self.summary_ms += other.summary_ms;
        for (cause, (count, tokens)) in &other.rebuilds {
            let entry = self.rebuilds.entry(cause.clone()).or_default();
            entry.0 += count;
            entry.1 += tokens;
        }
    }

    /// Rebuilds that could have been avoided (every cause but the start
    /// of a conversation): how many, and the tokens written.
    pub fn avoidable_rebuilds(&self) -> (u64, u64) {
        self.rebuilds
            .iter()
            .filter(|(cause, _)| cause.as_str() != cause::START)
            .fold((0, 0), |sum, (_, (count, tokens))| (sum.0 + count, sum.1 + tokens))
    }
}

/// What a [`Tally`] counts. Each kind says what `count`, `a` and `b` hold.
pub mod kind {
    /// One hook command: runs, milliseconds waited, characters it added to
    /// the conversation.
    pub const HOOK: &str = "hook";
    /// A hook that blocked or was cancelled: times.
    pub const HOOK_TROUBLE: &str = "hook-trouble";
    /// What a tool returned: results, characters, results that were errors.
    pub const TOOL_OUTPUT: &str = "tool-output";
    /// A tool call that was refused, by who refused it: times.
    pub const DENIED: &str = "denied";
    /// Replies at one effort level: replies, output tokens, of which thinking.
    pub const EFFORT: &str = "effort";
    /// Replies made while a skill was in charge: replies, output tokens,
    /// tokens of conversation sent.
    pub const SKILL: &str = "by-skill";
    /// The same for a plugin.
    pub const PLUGIN: &str = "by-plugin";
    /// The same for an MCP server.
    pub const MCP: &str = "by-mcp";
    /// Requests the API refused, by status and type: times.
    pub const API_ERROR: &str = "api-error";
    /// Cache writes by lifetime (`1h` or `5m`): replies, tokens written.
    pub const CACHE_LIFE: &str = "cache-life";
    /// Finished turns: turns, milliseconds from prompt to finish.
    pub const TURNS: &str = "turns";
    /// A list sent at the start of every session (`skills`, `agents`):
    /// sessions, characters, entries.
    pub const LISTING: &str = "listing";
    /// Conversations summarised while idle, before their cache expired: times.
    pub const IDLE_SUMMARY: &str = "idle-summary";
    /// Replies cut short (`stream cut`, `truncated`): times.
    pub const CUT: &str = "cut";
}

/// One counted thing of a [`kind`], summed over a span of days.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tally {
    /// What was counted: a hook command, a tool, an effort level.
    pub name: String,
    /// How many.
    pub count: u64,
    /// The kind's first amount (see [`kind`]).
    pub a: u64,
    /// The kind's second amount.
    pub b: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct TallyRow {
    day: String,
    owner: String,
    #[serde(rename = "k")]
    kind: String,
    name: String,
    #[serde(default, rename = "n")]
    count: u64,
    #[serde(default)]
    a: u64,
    #[serde(default)]
    b: u64,
}

/// The length of a JSON value's text, without keeping the text: the bytes
/// of a string, or of every string inside a list or object. Image data is
/// left out, since a picture is not charged by its characters.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Len(u64);

impl<'de> Deserialize<'de> for Len {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Len, D::Error> {
        struct Visit;
        impl<'de> serde::de::Visitor<'de> for Visit {
            type Value = Len;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_str<E>(self, v: &str) -> Result<Len, E> {
                Ok(Len(v.len() as u64))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Len, E> {
                Ok(Len(0))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Len, E> {
                Ok(Len(0))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Len, E> {
                Ok(Len(0))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Len, E> {
                Ok(Len(0))
            }
            fn visit_unit<E>(self) -> Result<Len, E> {
                Ok(Len(0))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Len, A::Error> {
                let mut total = 0;
                while let Some(Len(n)) = seq.next_element()? {
                    total += n;
                }
                Ok(Len(total))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Len, A::Error> {
                let mut total = 0;
                while let Some(key) = map.next_key::<std::borrow::Cow<str>>()? {
                    if key == "data" || key == "type" {
                        map.next_value::<serde::de::IgnoredAny>()?;
                    } else {
                        total += map.next_value::<Len>()?.0;
                    }
                }
                Ok(Len(total))
            }
        }
        deserializer.deserialize_any(Visit)
    }
}

/// A short name for a hook from its command: the script and its
/// arguments, without the interpreter and the directories.
fn hook_label(command: &str) -> String {
    let words: Vec<&str> = command.split_whitespace().collect();
    let script = words.iter().position(|w| w.contains('/')).unwrap_or(0);
    let mut label: Vec<String> = Vec::new();
    for (i, word) in words.iter().enumerate().skip(script) {
        let word = word.trim_matches(|c| c == '"' || c == '\'');
        if word.starts_with("||") || word.starts_with("&&") || word == ">/dev/null" {
            break;
        }
        label.push(if i == script || word.contains('/') {
            word.rsplit('/').next().unwrap_or(word).to_string()
        } else {
            word.to_string()
        });
    }
    let label = label.join(" ");
    if label.is_empty() {
        "(unnamed)".into()
    } else {
        label.chars().take(48).collect()
    }
}

/// [`Waste`] for one account, day and project.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WasteRow {
    /// The local calendar day, as `YYYY-MM-DD`.
    pub day: String,
    /// The account uuid the transcript was on, as in [`Row::owner`].
    pub owner: String,
    /// The project, as in [`Row::project`].
    pub project: String,
    /// The counts for this combination.
    #[serde(flatten)]
    pub waste: Waste,
}

/// Sums for one account, day, model, project and (for a subagent) agent type.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// The local calendar day of the replies, as `YYYY-MM-DD`.
    pub day: String,
    /// The account uuid from the transcript's latest `session-owner` line.
    /// Empty when the transcript had not named an account yet.
    pub owner: String,
    /// The model id as the transcript gives it; see [`model_name`] for the
    /// short form.
    pub model: String,
    /// The last two parts of the working directory the reply ran in, or
    /// `(unknown)` when the transcript gave none.
    pub project: String,
    /// Empty for the main conversation, else the subagent's type.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    /// The token counts for this combination.
    #[serde(flatten)]
    pub tokens: Tokens,
}

/// Successful hook runs for one account, day and hook event.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HookRow {
    /// The local calendar day of the runs, as `YYYY-MM-DD`.
    pub day: String,
    /// The account uuid the transcript was on, as in [`Row::owner`].
    pub owner: String,
    /// The hook event name, such as `PreToolUse`.
    pub event: String,
    /// How many hook runs were recorded as successful.
    pub runs: u64,
    /// Total duration of those runs, in milliseconds.
    pub ms: u64,
}

/// Tool calls for one account, day and tool.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolRow {
    /// The local calendar day of the calls, as `YYYY-MM-DD`.
    pub day: String,
    /// The account uuid the transcript was on, as in [`Row::owner`].
    pub owner: String,
    /// The tool's name as the model called it.
    pub tool: String,
    /// How many times the model called the tool.
    pub calls: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct FileSums {
    size: u64,
    offset: u64,
    /// The account the transcript was on at `offset`.
    #[serde(default)]
    owner: String,
    #[serde(default)]
    last_request: String,
    #[serde(default)]
    last_model: String,
    #[serde(default)]
    last_ms: i64,
    #[serde(default)]
    rows: Vec<Row>,
    #[serde(default)]
    hooks: Vec<HookRow>,
    #[serde(default)]
    tools: Vec<ToolRow>,
    #[serde(default)]
    compactions: Vec<(String, String)>,
    #[serde(default)]
    waste: Vec<WasteRow>,
    /// Skill invocations, with the skill's name in `tool`.
    #[serde(default)]
    skills: Vec<ToolRow>,
    /// Edits per file in this conversation: the day of the latest and how many.
    #[serde(default)]
    edits: BTreeMap<String, (String, u32)>,
    /// Tokens written since the user's last prompt.
    #[serde(default)]
    turn_output: u64,
    #[serde(default)]
    after_summary: bool,
    #[serde(default)]
    after_stop: bool,
    #[serde(default)]
    last_project: String,
    #[serde(default)]
    tally: Vec<TallyRow>,
}

/// The sums for every transcript changed in the last [`DAYS`] days, keyed by
/// transcript path. This is what `activity.json` holds. It contains counts,
/// account uuids, model ids and project names, and no message text.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Activity {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    files: BTreeMap<String, FileSums>,
}

// ---- the lines cx reads ------------------------------------------------------

#[derive(Deserialize)]
struct OwnerLine {
    #[serde(rename = "accountUuid", default)]
    account_uuid: String,
}

#[derive(Deserialize, Default)]
struct UsageJson {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation: CacheLife,
    #[serde(default)]
    output_tokens_details: OutputDetails,
}

#[derive(Deserialize, Default)]
struct CacheLife {
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
}

#[derive(Deserialize, Default)]
struct OutputDetails {
    #[serde(default)]
    thinking_tokens: u64,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    input: BlockInput,
}

#[derive(Deserialize, Default)]
struct BlockInput {
    #[serde(default)]
    skill: String,
    #[serde(default)]
    file_path: String,
    #[serde(default)]
    notebook_path: String,
}

#[derive(Deserialize, Default)]
struct UserMessage {
    #[serde(default)]
    content: serde_json::Value,
}

#[derive(Deserialize)]
struct UserLine {
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    cwd: String,
    #[serde(rename = "isMeta", default)]
    is_meta: bool,
    #[serde(rename = "isCompactSummary", default)]
    is_summary: bool,
    #[serde(default)]
    message: UserMessage,
}

#[derive(Deserialize, Default)]
struct CompactJson {
    #[serde(default)]
    trigger: String,
    #[serde(rename = "preTokens", default)]
    pre_tokens: u64,
    #[serde(rename = "durationMs", default)]
    duration_ms: f64,
}

#[derive(Deserialize)]
struct ResultBlock {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    tool_use_id: String,
    #[serde(default)]
    content: Len,
    #[serde(default)]
    is_error: bool,
}

#[derive(Deserialize, Default)]
struct ResultMessage {
    #[serde(default)]
    content: Vec<ResultBlock>,
}

#[derive(Deserialize)]
struct ResultLine {
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    message: ResultMessage,
    #[serde(rename = "toolDenialKind", default)]
    denial: Option<String>,
}

#[derive(Deserialize)]
struct TurnLine {
    #[serde(default)]
    timestamp: String,
    #[serde(rename = "durationMs", default)]
    duration_ms: f64,
}

#[derive(Deserialize)]
struct CompactLine {
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    cwd: String,
    #[serde(rename = "compactMetadata", default)]
    meta: CompactJson,
}

/// Whether a prompt opens by saying the previous turn was wrong. Only the
/// first words are looked at, and nothing of the prompt is kept.
fn reads_as_correction(text: &str) -> bool {
    let head: String = text.trim_start().chars().take(160).collect::<String>().to_lowercase();
    const OPENS: [&str; 14] = [
        "no,",
        "no ",
        "no.",
        "nope",
        "stop",
        "wrong",
        "that's not",
        "thats not",
        "not what",
        "i said",
        "i told",
        "undo",
        "revert",
        "again,",
    ];
    const HOLDS: [&str; 12] = [
        "i said",
        "i told you",
        "not what i",
        "you didn't",
        "you didnt",
        "you did not",
        "why did you",
        "still not",
        "still broken",
        "didn't work",
        "doesn't work",
        "i asked",
    ];
    OPENS.iter().any(|w| head.starts_with(w)) || HOLDS.iter().any(|w| head.contains(w))
}

/// The text of a user message, whether it is a plain string or a list of
/// blocks. `None` when it carries no text (a tool result, an image alone).
fn prompt_text(content: &serde_json::Value) -> Option<&str> {
    match content {
        serde_json::Value::String(text) => Some(text),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .and_then(|b| b.get("text"))
            .and_then(|t| t.as_str()),
        _ => None,
    }
}

#[derive(Deserialize, Default)]
struct MessageJson {
    #[serde(default)]
    model: String,
    #[serde(default)]
    usage: Option<UsageJson>,
    #[serde(default)]
    content: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct AssistantLine {
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    cwd: String,
    #[serde(rename = "requestId", default)]
    request_id: String,
    #[serde(rename = "attributionAgent", default)]
    attribution_agent: Option<String>,
    #[serde(rename = "attributionSkill", default)]
    by_skill: Option<String>,
    #[serde(rename = "attributionPlugin", default)]
    by_plugin: Option<String>,
    #[serde(rename = "attributionMcpServer", default)]
    by_mcp: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(rename = "apiErrorStatus", default)]
    error_status: Option<serde_json::Value>,
    #[serde(rename = "apiErrorType", default)]
    error_type: Option<String>,
    #[serde(rename = "streamCutCause", default)]
    cut: Option<String>,
    #[serde(rename = "truncatedAfterOutput", default)]
    truncated: bool,
    #[serde(default)]
    message: MessageJson,
}

#[derive(Deserialize, Default)]
struct HookJson {
    #[serde(rename = "hookEvent", default)]
    event: String,
    #[serde(rename = "durationMs", default)]
    duration_ms: f64,
    #[serde(rename = "hookName", default)]
    name: String,
    #[serde(default)]
    command: String,
    /// What the hook added to the conversation.
    #[serde(default)]
    content: Len,
    #[serde(rename = "skillCount", default)]
    skill_count: u64,
    #[serde(rename = "isInitial", default)]
    initial: bool,
    #[serde(rename = "addedLines", default)]
    added_lines: Vec<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct HookLine {
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    attachment: HookJson,
}

fn find(haystack: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(haystack, needle).is_some()
}

/// The local date of an RFC 3339 timestamp, and its time in milliseconds.
fn day_of(timestamp: &str) -> Option<(String, i64)> {
    let at = chrono::DateTime::parse_from_rfc3339(timestamp).ok()?;
    Some((
        at.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string(),
        at.timestamp_millis(),
    ))
}

/// The last two parts of a working directory: enough to tell projects apart.
fn project_of(cwd: &str) -> String {
    let parts: Vec<&str> = cwd.split('/').filter(|p| !p.is_empty()).collect();
    match parts.len() {
        0 => "(unknown)".into(),
        1 => parts[0].into(),
        n => format!("{}/{}", parts[n - 2], parts[n - 1]),
    }
}

/// "claude-fable-5-2" reads "Fable 5.2"; a trailing date is dropped.
pub fn model_name(id: &str) -> String {
    let parts: Vec<&str> = id
        .trim_start_matches("claude-")
        .split('-')
        .filter(|p| !(p.len() == 8 && p.chars().all(|c| c.is_ascii_digit())))
        .collect();
    let words: Vec<String> = parts
        .iter()
        .filter(|p| !p.chars().all(|c| c.is_ascii_digit()))
        .map(|p| {
            let mut chars = p.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    let numbers: Vec<&str> = parts
        .iter()
        .filter(|p| p.chars().all(|c| c.is_ascii_digit()))
        .copied()
        .collect();
    if numbers.is_empty() {
        words.join(" ")
    } else {
        format!("{} {}", words.join(" "), numbers.join("."))
    }
}

/// Day, account, kind and name.
type TallyKey = (String, String, &'static str, String);

struct Pending {
    rows: HashMap<(String, String, String, String, String), Tokens>,
    hooks: HashMap<(String, String, String), (u64, u64)>,
    tools: HashMap<(String, String, String), u64>,
    skills: HashMap<(String, String, String), u64>,
    waste: HashMap<(String, String, String), Waste>,
    /// Day, account, kind and name, to count and two amounts.
    tally: HashMap<TallyKey, (u64, u64, u64)>,
    /// Tool call ids to tool names, to name the tool a result belongs to.
    ids: HashMap<String, String>,
}

impl Pending {
    fn count(&mut self, day: &str, owner: &str, kind: &'static str, name: &str, a: u64, b: u64) {
        let entry = self
            .tally
            .entry((day.to_string(), owner.to_string(), kind, name.to_string()))
            .or_default();
        entry.0 += 1;
        entry.1 += a;
        entry.2 += b;
    }
}

impl FileSums {
    /// Read what was added to the file since the last read.
    fn read_more(&mut self, path: &Path, size: u64, agent_type: &str, oldest_day: &str) {
        if size < self.offset {
            // The file was replaced by a shorter one: start again.
            *self = FileSums::default();
        }
        let Ok(mut file) = std::fs::File::open(path) else {
            return;
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut pending = Pending {
            rows: HashMap::new(),
            hooks: HashMap::new(),
            tools: HashMap::new(),
            skills: HashMap::new(),
            waste: HashMap::new(),
            tally: HashMap::new(),
            ids: HashMap::new(),
        };
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(self.last_request.clone());
        let mut reader = BufReader::with_capacity(1 << 20, &mut file);
        let mut line = Vec::new();
        loop {
            line.clear();
            let Ok(read) = reader.read_until(b'\n', &mut line) else {
                break;
            };
            // A line still being written has no newline yet: leave it for next time.
            if read == 0 || line.last() != Some(&b'\n') {
                break;
            }
            self.offset += read as u64;
            // Most lines are not ones cx reads, and some are megabytes long:
            // a byte search decides before anything is parsed.
            if line.len() < 400 && find(&line, b"\"type\":\"session-owner\"") {
                if let Ok(owner) = serde_json::from_slice::<OwnerLine>(&line) {
                    self.owner = owner.account_uuid;
                }
            } else if find(&line, b"\"type\":\"assistant\"") {
                self.reply(&line, agent_type, oldest_day, &mut pending, &mut seen);
            } else if find(&line, b"\"type\":\"hook_success\"") {
                if let Ok(hook) = serde_json::from_slice::<HookLine>(&line)
                    && let Some((day, _)) = day_of(&hook.timestamp)
                    && day.as_str() >= oldest_day
                {
                    let label = format!("{} · {}", hook.attachment.name, hook_label(&hook.attachment.command));
                    pending.count(
                        &day,
                        &self.owner,
                        kind::HOOK,
                        &label,
                        hook.attachment.duration_ms.max(0.0) as u64,
                        hook.attachment.content.0,
                    );
                    let entry = pending
                        .hooks
                        .entry((day, self.owner.clone(), hook.attachment.event))
                        .or_default();
                    entry.0 += 1;
                    entry.1 += hook.attachment.duration_ms.max(0.0) as u64;
                }
            } else if find(&line, b"\"subtype\":\"compact_boundary\"") {
                if let Ok(compact) = serde_json::from_slice::<CompactLine>(&line)
                    && let Some((day, _)) = day_of(&compact.timestamp)
                {
                    self.after_summary = true;
                    if day.as_str() >= oldest_day {
                        self.compactions.push((day.clone(), self.owner.clone()));
                        let project = self.project_for(&compact.cwd);
                        let waste = pending.waste.entry((day, self.owner.clone(), project)).or_default();
                        if compact.meta.trigger == "manual" {
                            waste.asked_summaries += 1;
                        } else {
                            waste.auto_summaries += 1;
                        }
                        waste.summarised += compact.meta.pre_tokens;
                        waste.summary_ms += compact.meta.duration_ms.max(0.0) as u64;
                    }
                }
            } else if find(&line, b"\"type\":\"user\"") {
                if find(&line, b"\"type\":\"tool_result\"") {
                    self.tool_result(&line, oldest_day, &mut pending);
                } else if agent_type.is_empty() {
                    self.prompt(&line, oldest_day, &mut pending);
                }
            } else if find(&line, b"\"type\":\"attachment\"") {
                self.attachment(&line, oldest_day, &mut pending);
            } else if agent_type.is_empty()
                && find(&line, b"\"subtype\":\"turn_duration\"")
                && let Ok(turn) = serde_json::from_slice::<TurnLine>(&line)
                && let Some((day, _)) = day_of(&turn.timestamp)
                && day.as_str() >= oldest_day
            {
                pending.count(&day, &self.owner, kind::TURNS, "", turn.duration_ms.max(0.0) as u64, 0);
            } else if line.len() < 2000
                && find(&line, b"Compacted while idle")
                && find(&line, b"\"subtype\":\"informational\"")
                && let Ok(note) = serde_json::from_slice::<TurnLine>(&line)
                && let Some((day, _)) = day_of(&note.timestamp)
                && day.as_str() >= oldest_day
            {
                pending.count(&day, &self.owner, kind::IDLE_SUMMARY, "", 0, 0);
            }
        }
        self.size = size;
        for ((day, owner, model, project, agent), tokens) in pending.rows {
            match self.rows.iter_mut().find(|r| {
                r.day == day && r.owner == owner && r.model == model && r.project == project && r.agent == agent
            }) {
                Some(row) => row.tokens.add(&tokens),
                None => self.rows.push(Row {
                    day,
                    owner,
                    model,
                    project,
                    agent,
                    tokens,
                }),
            }
        }
        for ((day, owner, event), (runs, ms)) in pending.hooks {
            match self
                .hooks
                .iter_mut()
                .find(|h| h.day == day && h.owner == owner && h.event == event)
            {
                Some(row) => {
                    row.runs += runs;
                    row.ms += ms;
                }
                None => self.hooks.push(HookRow {
                    day,
                    owner,
                    event,
                    runs,
                    ms,
                }),
            }
        }
        for ((day, owner, project), waste) in pending.waste {
            match self
                .waste
                .iter_mut()
                .find(|w| w.day == day && w.owner == owner && w.project == project)
            {
                Some(row) => row.waste.add(&waste),
                None => self.waste.push(WasteRow {
                    day,
                    owner,
                    project,
                    waste,
                }),
            }
        }
        for ((day, owner, kind, name), (count, a, b)) in pending.tally {
            match self
                .tally
                .iter_mut()
                .find(|t| t.day == day && t.owner == owner && t.kind == kind && t.name == name)
            {
                Some(row) => {
                    row.count += count;
                    row.a += a;
                    row.b += b;
                }
                None => self.tally.push(TallyRow {
                    day,
                    owner,
                    kind: kind.to_string(),
                    name,
                    count,
                    a,
                    b,
                }),
            }
        }
        for ((day, owner, tool), calls) in pending.skills {
            match self
                .skills
                .iter_mut()
                .find(|t| t.day == day && t.owner == owner && t.tool == tool)
            {
                Some(row) => row.calls += calls,
                None => self.skills.push(ToolRow {
                    day,
                    owner,
                    tool,
                    calls,
                }),
            }
        }
        for ((day, owner, tool), calls) in pending.tools {
            match self
                .tools
                .iter_mut()
                .find(|t| t.day == day && t.owner == owner && t.tool == tool)
            {
                Some(row) => row.calls += calls,
                None => self.tools.push(ToolRow {
                    day,
                    owner,
                    tool,
                    calls,
                }),
            }
        }
    }

    /// The project a line belongs to: its own working directory, or the
    /// one the conversation was last in.
    fn project_for(&mut self, cwd: &str) -> String {
        if !cwd.is_empty() {
            self.last_project = project_of(cwd);
        }
        if self.last_project.is_empty() {
            "(unknown)".into()
        } else {
            self.last_project.clone()
        }
    }

    /// What tools returned: how much text went into the conversation, how
    /// often it was an error, and calls that were refused.
    fn tool_result(&mut self, line: &[u8], oldest_day: &str, pending: &mut Pending) {
        let Ok(result) = serde_json::from_slice::<ResultLine>(line) else {
            return;
        };
        let Some((day, _)) = day_of(&result.timestamp) else {
            return;
        };
        if day.as_str() < oldest_day {
            return;
        }
        for block in result.message.content.iter().filter(|b| b.kind == "tool_result") {
            let tool = pending
                .ids
                .get(&block.tool_use_id)
                .cloned()
                .unwrap_or_else(|| "(earlier call)".into());
            if let Some(denial) = &result.denial {
                pending.count(&day, &self.owner, kind::DENIED, &format!("{denial} · {tool}"), 0, 0);
            } else {
                pending.count(
                    &day,
                    &self.owner,
                    kind::TOOL_OUTPUT,
                    &tool,
                    block.content.0,
                    block.is_error as u64,
                );
            }
        }
    }

    /// Hooks that blocked or were cancelled, and the lists sent at the
    /// start of a session.
    fn attachment(&mut self, line: &[u8], oldest_day: &str, pending: &mut Pending) {
        let trouble = if find(line, b"\"type\":\"hook_blocking_error\"") {
            "blocked"
        } else if find(line, b"\"type\":\"hook_cancelled\"") {
            "cancelled"
        } else if find(line, b"\"type\":\"hook_non_blocking_error\"") {
            "failed"
        } else {
            ""
        };
        let skills = trouble.is_empty() && find(line, b"\"type\":\"skill_listing\"");
        let agents = trouble.is_empty() && !skills && find(line, b"\"type\":\"agent_listing_delta\"");
        if trouble.is_empty() && !skills && !agents {
            return;
        }
        let Ok(hook) = serde_json::from_slice::<HookLine>(line) else {
            return;
        };
        let Some((day, _)) = day_of(&hook.timestamp) else {
            return;
        };
        if day.as_str() < oldest_day {
            return;
        }
        let a = hook.attachment;
        if !trouble.is_empty() {
            pending.count(
                &day,
                &self.owner,
                kind::HOOK_TROUBLE,
                &format!("{trouble} · {}", a.name),
                0,
                0,
            );
        } else if a.initial && skills {
            pending.count(&day, &self.owner, kind::LISTING, "skills", a.content.0, a.skill_count);
        } else if a.initial && agents {
            // The line's own length stands for the list's: it holds little else.
            pending.count(
                &day,
                &self.owner,
                kind::LISTING,
                "agents",
                line.len() as u64,
                a.added_lines.len() as u64,
            );
        }
    }

    /// A line the user wrote: a prompt, or the note Claude Code leaves when
    /// the user stops a turn.
    fn prompt(&mut self, line: &[u8], oldest_day: &str, pending: &mut Pending) {
        let Ok(user) = serde_json::from_slice::<UserLine>(line) else {
            return;
        };
        if user.is_meta || user.is_summary {
            return;
        }
        let Some(text) = prompt_text(&user.message.content) else {
            return;
        };
        let Some((day, _)) = day_of(&user.timestamp) else {
            return;
        };
        let counted = day.as_str() >= oldest_day;
        let project = self.project_for(&user.cwd);
        let key = (day, self.owner.clone(), project);
        if text.starts_with("[Request interrupted by user") {
            if counted {
                let waste = pending.waste.entry(key).or_default();
                if text.contains("for tool use") {
                    waste.tool_stops += 1;
                } else {
                    waste.stops += 1;
                }
                waste.lost_output += self.turn_output;
            }
            self.turn_output = 0;
            self.after_stop = true;
            return;
        }
        // Commands, their output and notices are wrapped in tags.
        if text.trim_start().starts_with('<') {
            return;
        }
        if counted {
            let waste = pending.waste.entry(key).or_default();
            waste.prompts += 1;
            if self.after_stop {
                waste.redirects += 1;
            } else if reads_as_correction(text) {
                waste.corrections += 1;
            }
        }
        self.after_stop = false;
        self.turn_output = 0;
    }

    fn reply(
        &mut self,
        line: &[u8],
        agent_type: &str,
        oldest_day: &str,
        pending: &mut Pending,
        seen: &mut HashSet<String>,
    ) {
        let Ok(reply) = serde_json::from_slice::<AssistantLine>(line) else {
            return;
        };
        if let Some(status) = &reply.error_status
            && let Some((day, _)) = day_of(&reply.timestamp)
            && day.as_str() >= oldest_day
        {
            let name = format!("{status} {}", reply.error_type.as_deref().unwrap_or(""));
            pending.count(&day, &self.owner, kind::API_ERROR, name.trim(), 0, 0);
        }
        let Some(usage) = reply.message.usage else { return };
        if reply.message.model.is_empty() || reply.message.model.starts_with('<') {
            return;
        }
        let Some((day, at_ms)) = day_of(&reply.timestamp) else {
            return;
        };
        let (before_ms, before_model) = (self.last_ms, self.last_model.clone());
        if at_ms >= self.last_ms {
            self.last_ms = at_ms;
            self.last_model = reply.message.model.clone();
        }
        if day.as_str() < oldest_day {
            self.after_summary = false;
            return;
        }
        // Tool calls are counted on every line: one reply is written as
        // several lines, one per block.
        for block in &reply.message.content {
            if let Ok(block) = Block::deserialize(block)
                && block.kind == "tool_use"
                && !block.name.is_empty()
            {
                if !block.id.is_empty() {
                    pending.ids.insert(block.id.clone(), block.name.clone());
                }
                if block.name == "Skill" && !block.input.skill.is_empty() {
                    *pending
                        .skills
                        .entry((day.clone(), self.owner.clone(), block.input.skill.clone()))
                        .or_default() += 1;
                }
                let edited = match block.name.as_str() {
                    "Edit" | "Write" | "MultiEdit" => block.input.file_path.as_str(),
                    "NotebookEdit" => block.input.notebook_path.as_str(),
                    _ => "",
                };
                if !edited.is_empty() {
                    let entry = self.edits.entry(edited.to_string()).or_default();
                    entry.0 = day.clone();
                    entry.1 += 1;
                }
                *pending
                    .tools
                    .entry((day.clone(), self.owner.clone(), block.name))
                    .or_default() += 1;
            }
        }
        // The token counts repeat on each of those lines: count them once.
        if !reply.request_id.is_empty() {
            if !seen.insert(reply.request_id.clone()) {
                return;
            }
            self.last_request = reply.request_id;
        }
        self.turn_output += usage.output_tokens;
        let sent = usage.input_tokens + usage.cache_read_input_tokens + usage.cache_creation_input_tokens;
        let owner = self.owner.clone();
        pending.count(
            &day,
            &owner,
            kind::EFFORT,
            reply.effort.as_deref().unwrap_or("(not recorded)"),
            usage.output_tokens,
            usage.output_tokens_details.thinking_tokens,
        );
        for (kind, name) in [
            (kind::SKILL, &reply.by_skill),
            (kind::PLUGIN, &reply.by_plugin),
            (kind::MCP, &reply.by_mcp),
        ] {
            if let Some(name) = name.as_deref().filter(|n| !n.is_empty()) {
                pending.count(&day, &owner, kind, name, usage.output_tokens, sent);
            }
        }
        for (life, tokens) in [
            ("1h", usage.cache_creation.ephemeral_1h_input_tokens),
            ("5m", usage.cache_creation.ephemeral_5m_input_tokens),
        ] {
            if tokens > 0 {
                pending.count(&day, &owner, kind::CACHE_LIFE, life, tokens, 0);
            }
        }
        if let Some(cause) = &reply.cut {
            pending.count(&day, &owner, kind::CUT, &format!("stream cut: {cause}"), 0, 0);
        }
        if reply.truncated {
            pending.count(&day, &owner, kind::CUT, "truncated after output", 0, 0);
        }
        let rebuilt = usage.cache_creation_input_tokens >= REBUILD_TOKENS
            && usage.cache_read_input_tokens < usage.cache_creation_input_tokens;
        let project = self.project_for(&reply.cwd);
        if rebuilt {
            let idle = at_ms - before_ms;
            let why = if before_ms == 0 {
                cause::START
            } else if self.after_summary {
                cause::COMPACT
            } else if idle >= LONG_IDLE_MS {
                cause::LONG_IDLE
            } else if idle >= SHORT_IDLE_MS {
                cause::IDLE
            } else if before_model != reply.message.model {
                cause::MODEL
            } else {
                cause::OTHER
            };
            // A subagent's cache has its own, shorter lifetime, so its
            // rebuilds are listed apart.
            let why = if agent_type.is_empty() || why == cause::START {
                why.to_string()
            } else {
                format!("{why}{}", cause::IN_SUBAGENT)
            };
            let entry = pending
                .waste
                .entry((day.clone(), self.owner.clone(), project.clone()))
                .or_default()
                .rebuilds
                .entry(why)
                .or_default();
            entry.0 += 1;
            entry.1 += usage.cache_creation_input_tokens;
        }
        self.after_summary = false;
        let agent = if agent_type.is_empty() {
            reply.attribution_agent.unwrap_or_default()
        } else {
            agent_type.to_string()
        };
        let key = (day, self.owner.clone(), reply.message.model, project, agent);
        pending.rows.entry(key).or_default().add(&Tokens {
            input: usage.input_tokens,
            output: usage.output_tokens,
            cache_write: usage.cache_creation_input_tokens,
            cache_read: usage.cache_read_input_tokens,
            requests: 1,
            rebuilds: rebuilt as u64,
            rebuilt: if rebuilt { usage.cache_creation_input_tokens } else { 0 },
        });
    }

    fn drop_before(&mut self, oldest_day: &str) {
        self.rows.retain(|r| r.day.as_str() >= oldest_day);
        self.hooks.retain(|r| r.day.as_str() >= oldest_day);
        self.tools.retain(|r| r.day.as_str() >= oldest_day);
        self.compactions.retain(|(day, _)| day.as_str() >= oldest_day);
        self.waste.retain(|r| r.day.as_str() >= oldest_day);
        self.tally.retain(|r| r.day.as_str() >= oldest_day);
        self.skills.retain(|r| r.day.as_str() >= oldest_day);
        self.edits.retain(|_, (day, _)| day.as_str() >= oldest_day);
    }
}

/// Transcripts changed in the last `DAYS` days: main ones first, then each
/// session's subagent runs with the agent type from its `.meta.json` beside it.
fn recent_transcripts(projects: &Path, since_ms: i64) -> Vec<(PathBuf, u64, String)> {
    let modified = |meta: &std::fs::Metadata| {
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    };
    let mut main = Vec::new();
    let mut sub = Vec::new();
    let Ok(projects) = std::fs::read_dir(projects) else {
        return main;
    };
    for project in projects.flatten() {
        let Ok(entries) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
                if modified(&meta) >= since_ms {
                    main.push((path, meta.len(), String::new()));
                }
            } else if meta.is_dir() {
                let Ok(agents) = std::fs::read_dir(path.join("subagents")) else {
                    continue;
                };
                for agent in agents.flatten() {
                    let path = agent.path();
                    let Ok(meta) = agent.metadata() else { continue };
                    if path.extension().is_some_and(|e| e == "jsonl") && modified(&meta) >= since_ms {
                        let kind = util::read_json(&path.with_extension("meta.json"))
                            .ok()
                            .flatten()
                            .and_then(|v| v.get("agentType").and_then(|t| t.as_str()).map(String::from))
                            .unwrap_or_else(|| "subagent".into());
                        sub.push((path, meta.len(), kind));
                    }
                }
            }
        }
    }
    main.extend(sub);
    main
}

fn oldest_day() -> String {
    (chrono::Local::now() - chrono::Duration::days(DAYS - 1))
        .format("%Y-%m-%d")
        .to_string()
}

/// Today's date in the local time zone, as `YYYY-MM-DD`: the form the `day`
/// fields use.
pub fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

impl Activity {
    fn cache_path(store_root: &Path) -> PathBuf {
        store_root.join("activity.json")
    }

    /// Read the saved sums from `activity.json` in `store_root`. Returns an
    /// empty `Activity` when the file is missing, unreadable or was written
    /// by a version that kept different sums, so the next
    /// [`refresh`](Activity::refresh) reads the transcripts from the start.
    pub fn load(store_root: &Path) -> Activity {
        util::read_json(&Self::cache_path(store_root))
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_value::<Activity>(v).ok())
            .filter(|a| a.version == CACHE_VERSION)
            .unwrap_or_default()
    }

    /// Bring the sums up to date with the transcripts under `claude_dir`
    /// and save them. Returns whether anything changed.
    pub fn refresh(&mut self, claude_dir: &Path, store_root: &Path) -> bool {
        let oldest = oldest_day();
        let since_ms = util::now_ms() - DAYS * 86_400_000;
        let found = recent_transcripts(&claude_dir.join("projects"), since_ms);
        let mut changed = false;
        let keep: HashSet<String> = found.iter().map(|(p, _, _)| p.to_string_lossy().into_owned()).collect();
        let before = self.files.len();
        self.files.retain(|path, _| keep.contains(path));
        changed |= self.files.len() != before;
        for (path, size, agent_type) in found {
            let key = path.to_string_lossy().into_owned();
            let known = self.files.get(&key);
            if known.is_some_and(|k| k.size == size) {
                continue;
            }
            let mut sums = known.cloned().unwrap_or_default();
            // A subagent run is on its session's account: the session is
            // the directory two levels up, and its transcript sits beside it.
            if !agent_type.is_empty() && sums.owner.is_empty() {
                let session = path
                    .parent()
                    .and_then(Path::parent)
                    .map(|dir| dir.with_extension("jsonl"));
                if let Some(parent) = session.and_then(|p| self.files.get(p.to_string_lossy().as_ref())) {
                    sums.owner = parent.owner.clone();
                }
            }
            sums.read_more(&path, size, &agent_type, &oldest);
            sums.drop_before(&oldest);
            self.files.insert(key, sums);
            changed = true;
        }
        if changed {
            self.version = CACHE_VERSION;
            if let Ok(value) = serde_json::to_value(&*self) {
                let _ = util::write_json(&Self::cache_path(store_root), &value, 0o600);
            }
        }
        changed
    }

    /// The model of the newest reply in any transcript, and when it was.
    pub fn latest_model(&self) -> Option<(String, i64)> {
        self.files
            .values()
            .filter(|f| !f.last_model.is_empty())
            .max_by_key(|f| f.last_ms)
            .map(|f| (f.last_model.clone(), f.last_ms))
    }

    /// One account's sums for each of the last `DAYS` days, oldest first.
    pub fn daily(&self, owner: &str) -> Vec<(String, Tokens)> {
        let mut days: Vec<(String, Tokens)> = (0..DAYS)
            .rev()
            .map(|ago| {
                let day = chrono::Local::now() - chrono::Duration::days(ago);
                (day.format("%Y-%m-%d").to_string(), Tokens::default())
            })
            .collect();
        for row in self
            .files
            .values()
            .flat_map(|f| f.rows.iter())
            .filter(|r| r.owner == owner)
        {
            if let Some(slot) = days.iter_mut().find(|d| d.0 == row.day) {
                slot.1.add(&row.tokens);
            }
        }
        days
    }

    /// Everything one account did from `since_day` on.
    pub fn summary(&self, owner: &str, since_day: &str) -> Summary {
        self.summarise(Some(owner), since_day)
    }

    /// Everything every account did from `since_day` on, together.
    pub fn summary_all(&self, since_day: &str) -> Summary {
        self.summarise(None, since_day)
    }

    /// The account uuids that have any activity.
    pub fn owners(&self) -> Vec<String> {
        let owners: std::collections::BTreeSet<&str> = self
            .files
            .values()
            .flat_map(|f| f.rows.iter().map(|r| r.owner.as_str()))
            .filter(|o| !o.is_empty())
            .collect();
        owners.into_iter().map(String::from).collect()
    }

    /// The conversations from `since_day` on in which work was stopped,
    /// corrected, summarised or re-cached, the most wasteful first.
    pub fn sessions(&self, since_day: &str) -> Vec<Session> {
        let mut sessions: Vec<Session> = self
            .files
            .iter()
            .filter(|(path, _)| !path.contains("/subagents/"))
            .filter_map(|(path, file)| {
                let mut waste = Waste::default();
                for row in file.waste.iter().filter(|r| r.day.as_str() >= since_day) {
                    waste.add(&row.waste);
                }
                let any = waste.stops + waste.tool_stops + waste.redirects + waste.corrections > 0
                    || waste.auto_summaries + waste.asked_summaries > 0
                    || waste.avoidable_rebuilds().0 > 0;
                any.then(|| Session {
                    path: path.clone(),
                    project: file.last_project.clone(),
                    owner: file.owner.clone(),
                    last_ms: file.last_ms,
                    waste,
                })
            })
            .collect();
        sessions.sort_by_key(|s| std::cmp::Reverse(s.waste.lost_output + s.waste.avoidable_rebuilds().1));
        sessions
    }

    fn summarise(&self, only: Option<&str>, since_day: &str) -> Summary {
        let is = |owner: &str| only.is_none_or(|o| o == owner);
        let mut summary = Summary::default();
        let mut skills: BTreeMap<String, u64> = BTreeMap::new();
        let mut by_project: BTreeMap<String, Waste> = BTreeMap::new();
        let mut tallies: BTreeMap<String, BTreeMap<String, Tally>> = BTreeMap::new();
        let mut models: BTreeMap<String, Tokens> = BTreeMap::new();
        let mut projects: BTreeMap<String, Tokens> = BTreeMap::new();
        let mut agents: BTreeMap<String, Tokens> = BTreeMap::new();
        let mut hooks: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        let mut tools: BTreeMap<String, u64> = BTreeMap::new();
        for file in self.files.values() {
            for row in file.rows.iter().filter(|r| is(&r.owner) && r.day.as_str() >= since_day) {
                summary.total.add(&row.tokens);
                models.entry(model_name(&row.model)).or_default().add(&row.tokens);
                projects.entry(row.project.clone()).or_default().add(&row.tokens);
                if row.agent.is_empty() {
                    summary.main.add(&row.tokens);
                } else {
                    summary.subagents.add(&row.tokens);
                    agents.entry(row.agent.clone()).or_default().add(&row.tokens);
                }
            }
            for row in file
                .hooks
                .iter()
                .filter(|r| is(&r.owner) && r.day.as_str() >= since_day)
            {
                let entry = hooks.entry(row.event.clone()).or_default();
                entry.0 += row.runs;
                entry.1 += row.ms;
            }
            for row in file
                .tools
                .iter()
                .filter(|r| is(&r.owner) && r.day.as_str() >= since_day)
            {
                *tools.entry(row.tool.clone()).or_default() += row.calls;
            }
            summary.compactions += file
                .compactions
                .iter()
                .filter(|(day, who)| is(who) && day.as_str() >= since_day)
                .count() as u64;
            for row in file
                .waste
                .iter()
                .filter(|r| is(&r.owner) && r.day.as_str() >= since_day)
            {
                summary.waste.add(&row.waste);
                by_project.entry(row.project.clone()).or_default().add(&row.waste);
            }
            for row in file
                .skills
                .iter()
                .filter(|r| is(&r.owner) && r.day.as_str() >= since_day)
            {
                *skills.entry(row.tool.clone()).or_default() += row.calls;
            }
            for row in file
                .tally
                .iter()
                .filter(|r| is(&r.owner) && r.day.as_str() >= since_day)
            {
                let tally = tallies
                    .entry(row.kind.clone())
                    .or_default()
                    .entry(row.name.clone())
                    .or_default();
                tally.name.clone_from(&row.name);
                tally.count += row.count;
                tally.a += row.a;
                tally.b += row.b;
            }
            if is(&file.owner) {
                summary.churn.extend(
                    file.edits
                        .iter()
                        .filter(|(_, (day, edits))| *edits >= CHURN_EDITS && day.as_str() >= since_day)
                        .map(|(path, (_, edits))| (path.clone(), *edits)),
                );
            }
        }
        summary.tallies = tallies
            .into_iter()
            .map(|(kind, rows)| {
                let mut rows: Vec<Tally> = rows.into_values().collect();
                // Biggest first, by the first amount and then by how many.
                rows.sort_by(|x, y| (y.a, y.count).cmp(&(x.a, x.count)).then_with(|| x.name.cmp(&y.name)));
                (kind, rows)
            })
            .collect();
        summary.skills = skills.into_iter().collect();
        summary.skills.sort_by_key(|s| std::cmp::Reverse(s.1));
        summary.churn.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        summary.waste_by_project = by_project.into_iter().collect();
        summary
            .waste_by_project
            .sort_by_key(|(_, w)| std::cmp::Reverse(w.lost_output + w.avoidable_rebuilds().1));
        let by_weight = |map: BTreeMap<String, Tokens>| {
            let mut rows: Vec<(String, Tokens)> = map.into_iter().collect();
            rows.sort_by(|a, b| b.1.weight().total_cmp(&a.1.weight()));
            rows
        };
        summary.models = by_weight(models);
        summary.projects = by_weight(projects);
        summary.agents = by_weight(agents);
        summary.hooks = hooks.into_iter().map(|(event, (runs, ms))| (event, runs, ms)).collect();
        summary.hooks.sort_by_key(|h| std::cmp::Reverse(h.2));
        summary.tools = tools.into_iter().collect();
        summary.tools.sort_by_key(|t| std::cmp::Reverse(t.1));
        summary
    }
}

/// One conversation's waste, as listed by [`Activity::sessions`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Session {
    /// The transcript file.
    pub path: String,
    /// The project the conversation was last in.
    pub project: String,
    /// The account uuid the conversation was last on.
    pub owner: String,
    /// When its newest reply was written, in milliseconds since the epoch.
    pub last_ms: i64,
    /// What was stopped, corrected, summarised and re-cached in it.
    pub waste: Waste,
}

/// A point in a conversation where the user stopped Claude or corrected
/// it, with the text around it. Built on request by [`moments`] for the
/// person or agent working out why; never stored.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Moment {
    /// `stopped while replying`, `stopped a tool` or `correction`.
    pub kind: String,
    /// When, as the transcript's timestamp.
    pub at: String,
    /// The start of the prompt Claude was working on.
    pub asked: String,
    /// What Claude was doing: its last tool calls or the start of its text.
    pub doing: String,
    /// Tokens Claude had written in the turn up to this point.
    pub output: u64,
    /// The start of what the user typed next.
    pub then: String,
}

fn clip(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        flat.chars().take(max).collect::<String>() + "…"
    } else {
        flat
    }
}

/// The last `limit` moments in one transcript where the user stopped a
/// turn or corrected the one before. Unlike the sums, this reads message
/// text: it is meant for a report shown to the user on their own machine.
pub fn moments(path: &Path, limit: usize) -> Vec<Moment> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut moments: Vec<Moment> = Vec::new();
    let (mut asked, mut doing, mut output) = (String::new(), Vec::<String>::new(), 0u64);
    let mut open: Option<usize> = None;
    let mut seen: HashSet<String> = HashSet::new();
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if find(&line, b"\"type\":\"assistant\"") {
            let Ok(reply) = serde_json::from_slice::<AssistantLine>(&line) else {
                continue;
            };
            if let Some(usage) = &reply.message.usage
                && (reply.request_id.is_empty() || seen.insert(reply.request_id.clone()))
            {
                output += usage.output_tokens;
            }
            for block in &reply.message.content {
                let kind = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                let said = match kind {
                    "text" => block.get("text").and_then(|t| t.as_str()).map(|t| clip(t, 160)),
                    "tool_use" => {
                        let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        let input = block.get("input");
                        let detail = [
                            "command",
                            "file_path",
                            "description",
                            "skill",
                            "pattern",
                            "url",
                            "prompt",
                        ]
                        .iter()
                        .find_map(|key| input.and_then(|i| i.get(*key)).and_then(|v| v.as_str()))
                        .unwrap_or("");
                        Some(format!("{name}: {}", clip(detail, 120)))
                    }
                    _ => None,
                };
                if let Some(said) = said.filter(|s| !s.is_empty()) {
                    doing.push(said);
                    if doing.len() > 3 {
                        doing.remove(0);
                    }
                }
            }
        } else if find(&line, b"\"type\":\"user\"") && !find(&line, b"\"type\":\"tool_result\"") {
            let Ok(user) = serde_json::from_slice::<UserLine>(&line) else {
                continue;
            };
            if user.is_meta || user.is_summary {
                continue;
            }
            let Some(text) = prompt_text(&user.message.content) else {
                continue;
            };
            if text.starts_with("[Request interrupted by user") {
                let kind = if text.contains("for tool use") {
                    "stopped a tool"
                } else {
                    "stopped while replying"
                };
                moments.push(Moment {
                    kind: kind.into(),
                    at: user.timestamp,
                    asked: asked.clone(),
                    doing: doing.join(" → "),
                    output,
                    then: String::new(),
                });
                open = Some(moments.len() - 1);
                continue;
            }
            if text.trim_start().starts_with('<') {
                continue;
            }
            match open.take() {
                Some(index) => moments[index].then = clip(text, 300),
                None if reads_as_correction(text) => moments.push(Moment {
                    kind: "correction".into(),
                    at: user.timestamp,
                    asked: asked.clone(),
                    doing: doing.join(" → "),
                    output,
                    then: clip(text, 300),
                }),
                None => {}
            }
            asked = clip(text, 300);
            doing.clear();
            output = 0;
        }
    }
    let skip = moments.len().saturating_sub(limit);
    moments.split_off(skip)
}

/// One account's activity over a span of days, as built by
/// [`Activity::summary`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    /// Everything the account used: `main` plus `subagents`.
    pub total: Tokens,
    /// The part used by main conversations.
    pub main: Tokens,
    /// The part used by subagent runs.
    pub subagents: Tokens,
    /// Biggest first, by weighted tokens.
    pub models: Vec<(String, Tokens)>,
    /// Sums per project (see [`Row::project`]), biggest first by
    /// [`Tokens::weight`].
    pub projects: Vec<(String, Tokens)>,
    /// Sums per subagent type, biggest first by [`Tokens::weight`]. Main
    /// conversations are not listed here.
    pub agents: Vec<(String, Tokens)>,
    /// Event, runs, total milliseconds; slowest first.
    pub hooks: Vec<(String, u64, u64)>,
    /// Tool name and number of calls, most called first.
    pub tools: Vec<(String, u64)>,
    /// How many times a conversation was compacted.
    pub compactions: u64,
    /// Stopped turns, corrections, summaries and cache rebuilds.
    pub waste: Waste,
    /// The same per project, the most wasteful first.
    pub waste_by_project: Vec<(String, Waste)>,
    /// Skill name and number of times it was started, most used first.
    pub skills: Vec<(String, u64)>,
    /// Files edited [`CHURN_EDITS`] times or more in one conversation, with
    /// the number of edits, most edited first. A file listed twice was
    /// worked over in two conversations.
    pub churn: Vec<(String, u32)>,
    /// Everything else that was counted, by [`kind`], biggest first.
    pub tallies: BTreeMap<String, Vec<Tally>>,
}

impl Summary {
    /// The tallies of one [`kind`], biggest first; empty when none.
    pub fn tally(&self, kind: &str) -> &[Tally] {
        self.tallies.get(kind).map_or(&[], Vec::as_slice)
    }
}

/// The model Claude Code used last, from the tail of the newest transcript.
/// For callers that do not keep an `Activity` (the background service).
pub fn latest_model_quick(claude_dir: &Path) -> Option<String> {
    let mut newest: Option<(std::time::SystemTime, PathBuf, u64)> = None;
    for project in std::fs::read_dir(claude_dir.join("projects")).ok()?.flatten() {
        let Ok(entries) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "jsonl")
                && let Ok(meta) = entry.metadata()
                && let Ok(modified) = meta.modified()
                && newest.as_ref().is_none_or(|(at, _, _)| modified > *at)
            {
                newest = Some((modified, path, meta.len()));
            }
        }
    }
    let (_, path, size) = newest?;
    let mut file = std::fs::File::open(path).ok()?;
    const TAIL: u64 = 4 << 20;
    file.seek(SeekFrom::Start(size.saturating_sub(TAIL))).ok()?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes).ok()?;
    bytes
        .split(|b| *b == b'\n')
        .rev()
        .filter(|line| find(line, b"\"type\":\"assistant\""))
        .filter_map(|line| serde_json::from_slice::<AssistantLine>(line).ok())
        .map(|reply| reply.message.model)
        .find(|model| !model.is_empty() && !model.starts_with('<'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn stamp(hours_ago: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::hours(hours_ago)).to_rfc3339()
    }

    fn reply(request: &str, model: &str, cwd: &str, usage: (u64, u64, u64, u64), tool: Option<&str>) -> String {
        let content = match tool {
            Some(name) => json!([{"type": "tool_use", "name": name, "input": {"secret": "never read"}}]),
            None => json!([{"type": "text", "text": "never read"}]),
        };
        json!({"type": "assistant", "timestamp": stamp(1), "cwd": cwd, "requestId": request,
            "message": {"model": model, "content": content, "usage": {
                "input_tokens": usage.0, "output_tokens": usage.1,
                "cache_creation_input_tokens": usage.2, "cache_read_input_tokens": usage.3}}})
        .to_string()
    }

    fn world() -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("cx-activity-{}-{}", std::process::id(), fastrand::u32(..)));
        let claude = root.join("claude");
        let project = claude.join("projects").join("-work-app");
        std::fs::create_dir_all(&project).unwrap();
        let store = root.join("store");
        std::fs::create_dir_all(&store).unwrap();
        (claude, project, store)
    }

    #[test]
    fn replies_are_summed_per_account_model_and_project() {
        let (claude, project, store) = world();
        let lines = [
            json!({"type": "session-owner", "accountUuid": "acct-a", "sessionId": "s1"}).to_string(),
            reply(
                "r1",
                "claude-fable-5-2",
                "/work/app",
                (10, 100, 50_000, 0),
                Some("Bash"),
            ),
            // The same reply again, written for its second block.
            reply(
                "r1",
                "claude-fable-5-2",
                "/work/app",
                (10, 100, 50_000, 0),
                Some("Read"),
            ),
            reply("r2", "claude-fable-5-2", "/work/app", (5, 200, 100, 60_000), None),
            json!({"type": "attachment", "timestamp": stamp(1), "attachment":
                {"type": "hook_success", "hookEvent": "PreToolUse", "durationMs": 250}})
            .to_string(),
            json!({"type": "system", "subtype": "compact_boundary", "timestamp": stamp(1)}).to_string(),
            // The account changes mid-session.
            json!({"type": "session-owner", "accountUuid": "acct-b", "sessionId": "s1"}).to_string(),
            reply("r3", "claude-opus-5-5", "/work/site", (1, 2, 3, 4), None),
            json!({"type": "assistant", "timestamp": stamp(1), "message": {"model": "<synthetic>", "usage": {}}})
                .to_string(),
        ];
        std::fs::write(project.join("s1.jsonl"), lines.join("\n") + "\n").unwrap();
        let mut activity = Activity::default();
        assert!(activity.refresh(&claude, &store));

        let a = activity.summary("acct-a", &oldest_day());
        assert_eq!(
            a.total,
            Tokens {
                input: 15,
                output: 300,
                cache_write: 50_100,
                cache_read: 60_000,
                requests: 2,
                rebuilds: 1,
                rebuilt: 50_000,
            }
        );
        assert_eq!(a.models[0].0, "Fable 5.2");
        assert_eq!(a.projects[0].0, "work/app");
        assert_eq!(a.tools, vec![("Bash".to_string(), 1), ("Read".to_string(), 1)]);
        assert_eq!(a.hooks, vec![("PreToolUse".to_string(), 1, 250)]);
        assert_eq!(a.compactions, 1);
        assert_eq!(a.subagents, Tokens::default());

        let b = activity.summary("acct-b", &oldest_day());
        assert_eq!((b.total.requests, b.models[0].0.as_str()), (1, "Opus 5.5"));
        assert_eq!(activity.summary("nobody", &oldest_day()), Summary::default());
        assert_eq!(activity.latest_model().unwrap().0, "claude-opus-5-5");
        assert_eq!(latest_model_quick(&claude).as_deref(), Some("claude-opus-5-5"));
        // Nothing of the conversation itself reaches the saved sums.
        let saved = std::fs::read_to_string(store.join("activity.json")).unwrap();
        assert!(!saved.contains("never read"));
    }

    #[test]
    fn a_file_is_read_on_from_where_the_last_read_stopped() {
        let (claude, project, store) = world();
        let path = project.join("s1.jsonl");
        let first = [
            json!({"type": "session-owner", "accountUuid": "acct-a"}).to_string(),
            reply("r1", "claude-fable-5-2", "/work/app", (1, 10, 0, 0), None),
        ];
        std::fs::write(&path, first.join("\n") + "\n").unwrap();
        let mut activity = Activity::default();
        activity.refresh(&claude, &store);
        assert!(
            !activity.refresh(&claude, &store),
            "an unchanged file is not read again"
        );

        // A second block of r1, a new reply, and a line cut off mid-write.
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        let half = reply("r3", "claude-fable-5-2", "/work/app", (1, 1000, 0, 0), None);
        write!(
            file,
            "{}\n{}\n{}",
            reply("r1", "claude-fable-5-2", "/work/app", (1, 10, 0, 0), None),
            reply("r2", "claude-fable-5-2", "/work/app", (1, 20, 0, 0), None),
            &half[..half.len() / 2]
        )
        .unwrap();
        let mut reloaded = Activity::load(&store);
        assert!(reloaded.refresh(&claude, &store));
        let total = reloaded.summary("acct-a", &oldest_day()).total;
        assert_eq!((total.requests, total.output), (2, 30));
        // The rest of the line arrives.
        writeln!(file, "{}", &half[half.len() / 2..]).unwrap();
        reloaded.refresh(&claude, &store);
        assert_eq!(reloaded.summary("acct-a", &oldest_day()).total.output, 1030);
    }

    #[test]
    fn subagent_runs_belong_to_their_sessions_account() {
        let (claude, project, store) = world();
        std::fs::write(
            project.join("s1.jsonl"),
            [
                json!({"type": "session-owner", "accountUuid": "acct-a"}).to_string(),
                reply("r1", "claude-fable-5-2", "/work/app", (1, 10, 0, 0), None),
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
        let agents = project.join("s1").join("subagents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("agent-x.jsonl"),
            reply("r9", "claude-haiku-5-5", "/work/app", (1, 40, 0, 0), None) + "\n",
        )
        .unwrap();
        std::fs::write(
            agents.join("agent-x.meta.json"),
            json!({"agentType": "Explore"}).to_string(),
        )
        .unwrap();
        let mut activity = Activity::default();
        activity.refresh(&claude, &store);
        let summary = activity.summary("acct-a", &oldest_day());
        assert_eq!((summary.main.output, summary.subagents.output), (10, 40));
        assert_eq!(summary.agents[0].0, "Explore");
        assert_eq!(summary.models.len(), 2);
        let daily = activity.daily("acct-a");
        assert_eq!(daily.len(), DAYS as usize);
        assert_eq!(daily.iter().map(|d| d.1.output).sum::<u64>(), 50);
    }

    fn user(text: &str, hours_ago: i64) -> String {
        json!({"type": "user", "timestamp": stamp(hours_ago), "cwd": "/work/app",
            "message": {"role": "user", "content": text}})
        .to_string()
    }

    #[test]
    fn stops_corrections_summaries_and_rebuild_causes_are_counted() {
        let (claude, project, store) = world();
        let path = project.join("s1.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        let edit = json!({"type": "assistant", "timestamp": stamp(1), "cwd": "/work/app", "requestId": "e",
            "message": {"model": "claude-fable-5-2", "content": [
                {"type": "tool_use", "name": "Edit", "input": {"file_path": "/work/app/src/a.rs"}}],
                "usage": {"input_tokens": 0, "output_tokens": 0}}})
        .to_string();
        let skill = json!({"type": "assistant", "timestamp": stamp(1), "cwd": "/work/app", "requestId": "s",
            "message": {"model": "claude-fable-5-2", "content": [
                {"type": "tool_use", "name": "Skill", "input": {"skill": "deploy"}}],
                "usage": {"input_tokens": 0, "output_tokens": 0}}})
        .to_string();
        let summary = json!({"type": "system", "subtype": "compact_boundary", "timestamp": stamp(1),
            "compactMetadata": {"trigger": "auto", "preTokens": 300_000, "durationMs": 4000}})
        .to_string();
        let tool_result = json!({"type": "user", "timestamp": stamp(1), "message": {"content": [
            {"type": "tool_result", "content": "no, I said stop"}]}})
        .to_string();
        let mut lines = vec![
            json!({"type": "session-owner", "accountUuid": "acct-a"}).to_string(),
            user("build the thing", 1),
            // The first reply of a conversation: a rebuild nobody could avoid.
            reply("r1", "claude-fable-5-2", "/work/app", (10, 700, 50_000, 0), None),
            user("[Request interrupted by user]", 1),
            user("build it in Rust instead", 1),
            reply("r2", "claude-fable-5-2", "/work/app", (10, 300, 100, 50_000), None),
            user("[Request interrupted by user for tool use]", 1),
            user("<command-name>/clear</command-name>", 1),
            user("carry on", 1),
            user("no, I said the other file", 1),
            tool_result,
            summary,
            reply("r3", "claude-fable-5-2", "/work/app", (10, 5, 60_000, 0), None),
            reply("r4", "claude-opus-5-5", "/work/app", (10, 5, 40_000, 0), None),
            skill,
        ];
        lines.extend(std::iter::repeat_n(edit, CHURN_EDITS as usize));
        for line in &lines {
            writeln!(file, "{line}").unwrap();
        }
        let mut activity = Activity::load(&store);
        activity.refresh(&claude, &store);
        let s = activity.summary("acct-a", "0000-00-00");
        let w = &s.waste;
        assert_eq!((w.prompts, w.stops, w.tool_stops), (4, 1, 1));
        assert_eq!(w.lost_output, 1000, "700 before the first stop, 300 before the second");
        assert_eq!((w.redirects, w.corrections), (2, 1));
        assert_eq!(
            (w.auto_summaries, w.asked_summaries, w.summarised, w.summary_ms),
            (1, 0, 300_000, 4000)
        );
        assert_eq!(w.rebuilds[cause::START], (1, 50_000));
        assert_eq!(w.rebuilds[cause::COMPACT], (1, 60_000));
        assert_eq!(w.rebuilds[cause::MODEL], (1, 40_000));
        assert_eq!(w.avoidable_rebuilds(), (2, 100_000));
        assert_eq!(s.skills, vec![("deploy".to_string(), 1)]);
        assert_eq!(s.churn, vec![("/work/app/src/a.rs".to_string(), CHURN_EDITS)]);
        assert_eq!(activity.summary_all("0000-00-00").waste, s.waste);
        let sessions = activity.sessions("0000-00-00");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].project, "work/app");
        // No prompt text reaches the saved sums.
        let saved = std::fs::read_to_string(store.join("activity.json")).unwrap();
        assert!(!saved.contains("Rust instead") && !saved.contains("I said"));

        let moments = moments(&path, 10);
        let kinds: Vec<&str> = moments.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(kinds, ["stopped while replying", "stopped a tool", "correction"]);
        assert_eq!(moments[0].asked, "build the thing");
        assert_eq!(moments[0].then, "build it in Rust instead");
        assert_eq!(moments[0].output, 700);
        assert_eq!(moments[1].then, "carry on");
        assert_eq!(moments[2].then, "no, I said the other file");
    }

    #[test]
    fn a_rebuild_after_a_pause_is_blamed_on_the_pause() {
        let (claude, project, store) = world();
        let mut file = std::fs::File::create(project.join("s1.jsonl")).unwrap();
        let at = |minutes_ago: i64, request: &str| {
            json!({"type": "assistant", "timestamp": (chrono::Utc::now() - chrono::Duration::minutes(minutes_ago)).to_rfc3339(),
                "cwd": "/work/app", "requestId": request,
                "message": {"model": "claude-fable-5-2", "content": [], "usage": {
                    "input_tokens": 1, "output_tokens": 1,
                    "cache_creation_input_tokens": 30_000, "cache_read_input_tokens": 0}}})
            .to_string()
        };
        for line in [at(200, "a"), at(100, "b"), at(90, "c"), at(89, "d")] {
            writeln!(file, "{line}").unwrap();
        }
        let mut activity = Activity::load(&store);
        activity.refresh(&claude, &store);
        let w = activity.summary_all("0000-00-00").waste;
        assert_eq!(w.rebuilds[cause::START].0, 1);
        assert_eq!(w.rebuilds[cause::LONG_IDLE].0, 1);
        assert_eq!(w.rebuilds[cause::IDLE].0, 1);
        assert_eq!(w.rebuilds[cause::OTHER].0, 1);
    }

    #[test]
    fn corrections_are_told_by_their_opening_words() {
        for text in [
            "No, the other one",
            "stop doing that",
            "that's not what I asked",
            "Why did you delete it?",
            "it still not working",
        ] {
            assert!(reads_as_correction(text), "{text}");
        }
        for text in [
            "now add tests",
            "note the port is 8080",
            "great, ship it",
            "know what to do next?",
        ] {
            assert!(!reads_as_correction(text), "{text}");
        }
    }

    #[test]
    fn model_ids_read_as_names() {
        assert_eq!(model_name("claude-fable-5-2"), "Fable 5.2");
        assert_eq!(model_name("claude-opus-5-5-20260101"), "Opus 5.5");
        assert_eq!(model_name("claude-haiku-5-5"), "Haiku 5.5");
        assert_eq!(model_name("custom"), "Custom");
        assert_eq!(project_of("/Users/x/work/app"), "work/app");
        assert_eq!(project_of(""), "(unknown)");
    }
}
