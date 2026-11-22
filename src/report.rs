//! `cx report`: everything cx knows, as JSON, for other tools to read.
//!
//! The dashboard is for people. This is the same information for a
//! program, and in particular for the cx plugin for Claude Code, which
//! reads it to advise on how the harness is set up: what each account's
//! limits are, what used them, and what was paid for and thrown away
//! (stopped turns, corrections, summaries, caches written again).
//!
//! It works with no account added to cx at all: the transcripts name the
//! accounts they ran on, and those are reported by a number.

use crate::activity::{self, Activity, Summary, Tokens, Waste};
use crate::hook;
use crate::store::{Account, Kind, State};
use serde_json::{Value, json};
use std::path::Path;

/// How many rows of each list the report carries.
const TOP: usize = 12;

fn tokens(t: &Tokens) -> Value {
    json!({
        "requests": t.requests,
        "output": t.output,
        "input_uncached": t.input,
        "cache_written": t.cache_write,
        "cache_read": t.cache_read,
    })
}

fn named(rows: &[(String, Tokens)], total: f64) -> Vec<Value> {
    rows.iter()
        .take(TOP)
        .map(|(name, t)| {
            let share = if total > 0.0 { t.weight() / total * 100.0 } else { 0.0 };
            let mut row = tokens(t);
            row["name"] = json!(name);
            row["share_pct"] = json!(share.round());
            row
        })
        .collect()
}

fn waste(w: &Waste) -> Value {
    let (avoidable, avoidable_tokens) = w.avoidable_rebuilds();
    let mut causes: Vec<(&String, &(u64, u64))> = w.rebuilds.iter().collect();
    causes.sort_by_key(|(_, (_, tokens))| std::cmp::Reverse(*tokens));
    json!({
        "prompts": w.prompts,
        "stopped_while_replying": w.stops,
        "stopped_or_refused_a_tool": w.tool_stops,
        "output_tokens_in_stopped_turns": w.lost_output,
        "prompts_right_after_a_stop": w.redirects,
        "prompts_worded_as_corrections": w.corrections,
        "summaries_started_by_claude_code": w.auto_summaries,
        "summaries_the_user_asked_for": w.asked_summaries,
        "tokens_in_conversations_when_summarised": w.summarised,
        "seconds_spent_summarising": w.summary_ms / 1000,
        "cache_rebuilds_that_could_be_avoided": {"count": avoidable, "tokens_written_again": avoidable_tokens},
        "cache_rebuilds_by_cause": causes
            .into_iter()
            .map(|(cause, (count, tokens))| json!({"cause": cause, "count": count, "tokens_written_again": tokens}))
            .collect::<Vec<_>>(),
    })
}

fn brief_waste(w: &Waste) -> Value {
    let (avoidable, avoidable_tokens) = w.avoidable_rebuilds();
    let worst = w
        .rebuilds
        .iter()
        .filter(|(cause, _)| cause.as_str() != activity::cause::START)
        .max_by_key(|(_, (_, tokens))| *tokens)
        .map(|(cause, _)| cause.as_str());
    json!({
        "prompts": w.prompts,
        "stops": w.stops + w.tool_stops,
        "output_tokens_in_stopped_turns": w.lost_output,
        "redirects_and_corrections": w.redirects + w.corrections,
        "summaries": w.auto_summaries + w.asked_summaries,
        "avoidable_cache_rebuilds": {"count": avoidable, "tokens_written_again": avoidable_tokens, "main_cause": worst},
    })
}

fn summary(s: &Summary) -> Value {
    let total = s.total.weight();
    json!({
        "total": tokens(&s.total),
        "main_conversations": tokens(&s.main),
        "subagents": tokens(&s.subagents),
        "by_model": named(&s.models, total),
        "by_project": named(&s.projects, total),
        "by_subagent_type": named(&s.agents, total),
        "tools": s.tools.iter().take(TOP).map(|(name, calls)| json!({"name": name, "calls": calls})).collect::<Vec<_>>(),
        "skills_started": s.skills.iter().take(TOP * 2).map(|(name, calls)| json!({"name": name, "calls": calls})).collect::<Vec<_>>(),
        "hooks": s.hooks.iter().map(|(event, runs, ms)| json!({
            "event": event, "runs": runs, "seconds_waited": ms / 1000,
            "ms_each": if *runs > 0 { ms / runs } else { 0 },
        })).collect::<Vec<_>>(),
        "waste": waste(&s.waste),
        "waste_by_project": s.waste_by_project.iter().take(TOP).map(|(project, w)| {
            let mut row = brief_waste(w);
            row["project"] = json!(project);
            row
        }).collect::<Vec<_>>(),
        "files_edited_over_and_over": s.churn.iter().take(TOP).map(|(file, edits)| json!({"file": file, "edits_in_one_conversation": edits})).collect::<Vec<_>>(),
    })
}

/// The short form used per account; the full breakdown is given once, for
/// every account together, to keep the report small enough to read whole.
fn brief(s: &Summary) -> Value {
    let (avoidable, avoidable_tokens) = s.waste.avoidable_rebuilds();
    json!({
        "total": tokens(&s.total),
        "by_model": named(&s.models, s.total.weight()),
        "prompts": s.waste.prompts,
        "stops": s.waste.stops + s.waste.tool_stops,
        "output_tokens_in_stopped_turns": s.waste.lost_output,
        "summaries": s.waste.auto_summaries + s.waste.asked_summaries,
        "avoidable_cache_rebuilds": {"count": avoidable, "tokens_written_again": avoidable_tokens},
    })
}

fn limits(state: &State, account: &Account, now_ms: i64) -> Value {
    let Some(record) = state.usage.get(&account.id) else {
        return Value::Null;
    };
    let Some(usage) = &record.usage else {
        return Value::Null;
    };
    let window = |w: &crate::store::Window| json!({"used_pct": w.pct_at(now_ms), "resets_at": w.resets_at});
    json!({
        "five_hours": usage.five_hour.as_ref().map(window),
        "week_all_models": usage.seven_day.as_ref().map(window),
        "week_single_model": usage.models.iter().map(|m| {
            let mut row = window(&m.window);
            row["model"] = json!(m.name);
            row
        }).collect::<Vec<_>>(),
        "read_minutes_ago": (now_ms - record.fetched_ms).max(0) / 60_000,
    })
}

/// Build the report: `days` is how far back the activity goes (1 for
/// today, up to [`activity::DAYS`]). `state` is cx's own state, `live` the
/// account Claude Code is signed in as (if cx manages it), and `activity`
/// must already be refreshed.
pub fn build(
    state: &State,
    live: Option<u32>,
    activity: &Activity,
    store_root: &Path,
    days: i64,
    now_ms: i64,
) -> Value {
    let days = days.clamp(1, activity::DAYS);
    let since = (chrono::Local::now() - chrono::Duration::days(days - 1))
        .format("%Y-%m-%d")
        .to_string();
    let mut accounts: Vec<Value> = state
        .accounts
        .values()
        .map(|account| {
            let used = (!account.uuid.is_empty()).then(|| brief(&activity.summary(&account.uuid, &since)));
            json!({
                "number": account.id,
                "name": account.alias.clone().unwrap_or_else(|| account.label.clone()),
                "kind": if account.kind == Kind::Oauth { "subscription" } else { "api key" },
                "plan": account.plan,
                "in_use_now": live == Some(account.id),
                "disabled": account.disabled,
                "needs_sign_in": account.needs_login,
                "limits": limits(state, account, now_ms),
                "used": used,
            })
        })
        .collect();
    // Accounts the transcripts name that cx does not manage: still worth
    // reporting, since the plugin works without any account added.
    let known: Vec<&str> = state.accounts.values().map(|a| a.uuid.as_str()).collect();
    for (index, owner) in activity
        .owners()
        .iter()
        .filter(|o| !known.contains(&o.as_str()))
        .enumerate()
    {
        accounts.push(json!({
            "name": format!("account {} (not added to cx)", index + 1),
            "kind": "subscription",
            "used": brief(&activity.summary(owner, &since)),
        }));
    }
    let name_of = |owner: &str| {
        state
            .accounts
            .values()
            .find(|a| !owner.is_empty() && a.uuid == owner)
            .map(|a| format!("#{}", a.id))
            .unwrap_or_default()
    };
    let sessions: Vec<Value> = activity
        .sessions(&since)
        .iter()
        .take(TOP)
        .map(|s| {
            let mut row = brief_waste(&s.waste);
            row["transcript"] = json!(s.path);
            row["project"] = json!(s.project);
            row["account"] = json!(name_of(&s.owner));
            row["last_reply_minutes_ago"] = json!((now_ms - s.last_ms).max(0) / 60_000);
            row
        })
        .collect();
    let events = hook::load(store_root, now_ms - days * 86_400_000);
    json!({
        "what": "cx report: Claude Code usage and waste on this machine",
        "days": days,
        "since": since,
        "model_claude_code_used_last": activity.latest_model().map(|m| activity::model_name(&m.0)),
        "accounts": accounts,
        "all_accounts_together": summary(&activity.summary_all(&since)),
        "most_wasteful_conversations": sessions,
        "from_the_plugin_hooks": hook::summary(&events, now_ms),
        "notes": [
            "Token counts come from Claude Code's transcripts on this machine only.",
            "A stopped turn's output is everything written from the prompt to the stop; some of it may still have been useful.",
            "Corrections are counted from a prompt's opening words, so that number is an estimate.",
            "A cache rebuild at the start of a conversation or subagent cannot be avoided and is left out of the avoidable total.",
            "Run `cx report --moments <transcript>` to see what was asked, what Claude was doing and what the user typed next at each stop or correction.",
        ],
    })
}

/// The stops and corrections in one transcript, with the text around them.
pub fn moments(transcript: &Path, limit: usize) -> Value {
    json!({
        "transcript": transcript,
        "moments": activity::moments(transcript, limit),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_machine_still_gets_a_report() {
        let root = std::env::temp_dir().join(format!("cx-report-{}-{}", std::process::id(), fastrand::u32(..)));
        std::fs::create_dir_all(&root).unwrap();
        let report = build(&State::default(), None, &Activity::default(), &root, 7, 0);
        assert_eq!(report["accounts"].as_array().unwrap().len(), 0);
        assert_eq!(report["all_accounts_together"]["waste"]["prompts"], 0);
        assert_eq!(report["from_the_plugin_hooks"]["recorded"], false);
    }
}
