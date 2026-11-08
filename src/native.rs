//! Native Claude Code sessions, watched through Claude Code's documented
//! hooks (experimental; see `docs/native-sessions.md`).
//!
//! `t3-watch hook claude-code` runs as an async command hook. It reads the
//! hook's JSON from stdin and appends the event's name and type, and
//! nothing else (no prompt, message, tool input or path), to
//! `<state>/native/claude-code/<session_id>.jsonl`. It never prints a
//! decision and never fails the hook, so it cannot change the session.
//! `claude_state` folds that log into the thread state the engine watches,
//! so waits, needs-input, webhooks and the durable outbox work as for T3.

use crate::config;
use crate::model::ThreadState;
use crate::util;
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::Instant;

/// Target prefix: `claude-code:<session_id>`.
pub const CLAUDE_PREFIX: &str = "claude-code:";
const MAX_HOOK_INPUT: u64 = 4 << 20;
/// Only the end of a long log is folded; a turn's events are near its end.
const MAX_LOG_READ: u64 = 8 << 20;

/// Notification types that mean the session waits on its user.
const NEEDS_INPUT: [&str; 4] = [
    "permission_prompt",
    "elicitation_dialog",
    "elicitation_url_dialog",
    "agent_needs_input",
];
/// Events that end what a pending request was waiting on. Tool events are
/// not among them: with tool ids deliberately not recorded, a later tool
/// event (a parallel call, an async hook arriving late) cannot be told
/// apart from the one that was waiting, so a request stays pending until
/// it is denied, its elicitation is answered, or its turn ends.
const ANSWERED: [&str; 2] = ["PermissionDenied", "ElicitationResult"];

pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn claude_thread_id(session: &str) -> String {
    format!("{CLAUDE_PREFIX}{session}")
}

fn claude_log(session: &str) -> PathBuf {
    config::state_dir()
        .join("native")
        .join("claude-code")
        .join(format!("{session}.jsonl"))
}

fn token(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty() && t.len() <= 64)
        .filter(|t| t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
        .map(str::to_string)
}

/// Records one hook invocation from `input` (the hook's stdin). An error is
/// for the caller to log; the hook still succeeds.
pub fn record_claude_hook(input: impl Read) -> Result<(), String> {
    let mut raw = Vec::new();
    input
        .take(MAX_HOOK_INPUT)
        .read_to_end(&mut raw)
        .map_err(|e| format!("cannot read the hook input: {e}"))?;
    let v: Value = serde_json::from_slice(&raw).map_err(|_| "the hook input is not JSON".to_string())?;
    let session = v
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| valid_session_id(s))
        .ok_or("the hook input has no valid session_id")?;
    let event = token(&v, "hook_event_name").ok_or("the hook input has no valid hook_event_name")?;
    let kind = ["notification_type", "source", "reason"]
        .iter()
        .find_map(|k| token(&v, k));
    let mut line = json!({
        "at": util::rfc3339_ms(util::now_ms()),
        "event": event,
        "type": kind,
        "subagent": v.get("agent_id").is_some_and(|a| !a.is_null()),
    })
    .to_string();
    line.push('\n');
    let path = claude_log(session);
    let mut f = util::open_log(&path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    f.write_all(line.as_bytes())
        .and_then(|_| f.sync_data())
        .map_err(|e| format!("cannot record the hook event: {e}"))
}

/// The hook entries to merge into a Claude Code settings file, running
/// `bin` asynchronously on every event the adapter folds.
pub fn claude_settings(bin: &str) -> Value {
    let command = format!("{} hook claude-code", shell_quote(bin));
    let handler = json!([{ "hooks": [{ "type": "command", "command": command, "async": true }] }]);
    let mut hooks = serde_json::Map::new();
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "Notification",
        "PermissionRequest",
        "PermissionDenied",
        "ElicitationResult",
        "Stop",
        "StopFailure",
        "SessionEnd",
    ] {
        hooks.insert(event.to_string(), handler.clone());
    }
    json!({ "hooks": hooks })
}

/// `s` as one word for the POSIX shell that runs command hooks.
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+' | b':' | b'='))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The session as its recorded hook events describe it, in the shape the
/// engine reads for any thread: a status, the latest run (one per prompt)
/// and how many requests are pending.
pub fn claude_state(session: &str, observed_at: Instant) -> ThreadState {
    let tid = claude_thread_id(session);
    // `pending` counts the requests seen since the last event that ended
    // them, so each new one gives the wait a new pendingKey.
    let (mut turn, mut status, mut pending, mut events) = (0u64, None::<&str>, 0u64, 0usize);
    for line in read_log_tail(&claude_log(session)).lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        events += 1;
        let event = v.get("event").and_then(Value::as_str).unwrap_or("");
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        // A subagent's stop or end is not the session's.
        let subagent = v.get("subagent").and_then(Value::as_bool) == Some(true);
        match event {
            "UserPromptSubmit" => {
                turn += 1;
                status = Some("running");
                pending = 0;
            }
            "Stop" | "StopFailure" | "SessionEnd" if subagent => {}
            "Stop" => {
                status = Some("completed");
                pending = 0;
            }
            "StopFailure" => {
                status = Some("failed");
                pending = 0;
            }
            "PermissionRequest" => pending += 1,
            "Notification" if NEEDS_INPUT.contains(&kind) => pending += 1,
            "Notification" if kind == "idle_prompt" && status.is_none() => status = Some("completed"),
            e if ANSWERED.contains(&e) => pending = 0,
            "SessionEnd" => {
                if status == Some("running") {
                    status = Some("cancelled");
                }
                pending = 0;
            }
            _ => {}
        }
    }
    let mut thread = json!({
        "threadId": tid,
        "status": status,
        "pendingRequestCount": pending,
        "title": if events == 0 {
            "Claude Code session (no hook events recorded yet)"
        } else {
            "Claude Code session"
        },
    });
    if turn > 0 {
        let run = format!("run:thread:{tid}:ordinal:{turn}");
        thread["latestRunId"] = json!(run);
        thread["runCount"] = json!(turn);
        if status == Some("running") {
            thread["activeRunId"] = json!(run);
        }
    }
    ThreadState::from_thread_json(&json!({ "thread": thread }), &tid, observed_at)
}

/// The end of the log, from its first whole line: empty when there is no
/// log, or it is not a regular file of ours.
fn read_log_tail(path: &std::path::Path) -> String {
    let Ok(mut f) = util::open_private_read(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(MAX_LOG_READ);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut text = String::new();
    let _ = f.take(MAX_LOG_READ).read_to_string(&mut text);
    if start > 0 {
        text = text
            .split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_narrow() {
        assert!(valid_session_id("00893aaf-19fa-41d2-8238-13269b9b3ca0"));
        for bad in ["", "a/b", "../x", "a b", "a?b", &"x".repeat(129)] {
            assert!(!valid_session_id(bad), "{bad}");
        }
    }

    #[test]
    fn hook_commands_quote_the_binary_path_for_the_shell() {
        assert_eq!(shell_quote("/usr/local/bin/t3-watch"), "/usr/local/bin/t3-watch");
        assert_eq!(shell_quote("/Users/a b/t3-watch"), "'/Users/a b/t3-watch'");
        assert_eq!(shell_quote("/x/it's"), r"'/x/it'\''s'");
        assert_eq!(
            claude_settings("/Users/a b/t3-watch")["hooks"]["Stop"][0]["hooks"][0]["command"],
            "'/Users/a b/t3-watch' hook claude-code"
        );
        // Tool events no longer change the folded state, so no hook for them.
        let s = claude_settings("/x");
        assert!(s["hooks"].get("PostToolUse").is_none() && s["hooks"].get("PostToolUseFailure").is_none());
    }

    #[test]
    fn settings_run_the_hook_asynchronously_on_every_folded_event() {
        let s = claude_settings("/opt/t3-watch");
        for event in [
            "UserPromptSubmit",
            "Stop",
            "StopFailure",
            "Notification",
            "PermissionRequest",
            "SessionEnd",
        ] {
            let h = &s["hooks"][event][0]["hooks"][0];
            assert_eq!(h["async"], true, "{event}");
            assert_eq!(h["command"], "/opt/t3-watch hook claude-code", "{event}");
        }
    }
}
