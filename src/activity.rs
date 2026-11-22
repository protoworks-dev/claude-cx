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
//! `~/.claude`, and no message text is read into the sums.

use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Bumped when what is kept per file changes, so old sums are read again.
const CACHE_VERSION: u32 = 2;
/// Days of activity kept and shown.
pub const DAYS: i64 = 7;
/// A reply that wrote this many tokens to the cache while reading fewer
/// than it wrote had to rebuild the conversation's cache.
const REBUILD_TOKENS: u64 = 20_000;

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
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    name: String,
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
    #[serde(default)]
    message: MessageJson,
}

#[derive(Deserialize, Default)]
struct HookJson {
    #[serde(rename = "hookEvent", default)]
    event: String,
    #[serde(rename = "durationMs", default)]
    duration_ms: f64,
}

#[derive(Deserialize)]
struct HookLine {
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    attachment: HookJson,
}

#[derive(Deserialize)]
struct StampLine {
    #[serde(default)]
    timestamp: String,
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

struct Pending {
    rows: HashMap<(String, String, String, String, String), Tokens>,
    hooks: HashMap<(String, String, String), (u64, u64)>,
    tools: HashMap<(String, String, String), u64>,
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
                    let entry = pending
                        .hooks
                        .entry((day, self.owner.clone(), hook.attachment.event))
                        .or_default();
                    entry.0 += 1;
                    entry.1 += hook.attachment.duration_ms.max(0.0) as u64;
                }
            } else if find(&line, b"\"subtype\":\"compact_boundary\"")
                && let Ok(stamp) = serde_json::from_slice::<StampLine>(&line)
                && let Some((day, _)) = day_of(&stamp.timestamp)
                && day.as_str() >= oldest_day
            {
                self.compactions.push((day, self.owner.clone()));
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
        let Some(usage) = reply.message.usage else { return };
        if reply.message.model.is_empty() || reply.message.model.starts_with('<') {
            return;
        }
        let Some((day, at_ms)) = day_of(&reply.timestamp) else {
            return;
        };
        if at_ms >= self.last_ms {
            self.last_ms = at_ms;
            self.last_model = reply.message.model.clone();
        }
        if day.as_str() < oldest_day {
            return;
        }
        // Tool calls are counted on every line: one reply is written as
        // several lines, one per block.
        for block in &reply.message.content {
            if let Ok(block) = Block::deserialize(block)
                && block.kind == "tool_use"
                && !block.name.is_empty()
            {
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
        let rebuilt = usage.cache_creation_input_tokens >= REBUILD_TOKENS
            && usage.cache_read_input_tokens < usage.cache_creation_input_tokens;
        let agent = if agent_type.is_empty() {
            reply.attribution_agent.unwrap_or_default()
        } else {
            agent_type.to_string()
        };
        let key = (
            day,
            self.owner.clone(),
            reply.message.model,
            project_of(&reply.cwd),
            agent,
        );
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
        let mut summary = Summary::default();
        let mut models: BTreeMap<String, Tokens> = BTreeMap::new();
        let mut projects: BTreeMap<String, Tokens> = BTreeMap::new();
        let mut agents: BTreeMap<String, Tokens> = BTreeMap::new();
        let mut hooks: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        let mut tools: BTreeMap<String, u64> = BTreeMap::new();
        for file in self.files.values() {
            for row in file
                .rows
                .iter()
                .filter(|r| r.owner == owner && r.day.as_str() >= since_day)
            {
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
                .filter(|r| r.owner == owner && r.day.as_str() >= since_day)
            {
                let entry = hooks.entry(row.event.clone()).or_default();
                entry.0 += row.runs;
                entry.1 += row.ms;
            }
            for row in file
                .tools
                .iter()
                .filter(|r| r.owner == owner && r.day.as_str() >= since_day)
            {
                *tools.entry(row.tool.clone()).or_default() += row.calls;
            }
            summary.compactions += file
                .compactions
                .iter()
                .filter(|(day, who)| who == owner && day.as_str() >= since_day)
                .count() as u64;
        }
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
