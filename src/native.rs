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
/// Events that come after a pending request was answered.
const ANSWERED: [&str; 4] = [
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionDenied",
    "ElicitationResult",
];

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
    let handler =
        json!([{ "hooks": [{ "type": "command", "command": format!("{bin} hook claude-code"), "async": true }] }]);
    let mut hooks = serde_json::Map::new();
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "Notification",
        "PermissionRequest",
        "PermissionDenied",
        "PostToolUse",
        "PostToolUseFailure",
        "ElicitationResult",
        "Stop",
        "StopFailure",
        "SessionEnd",
    ] {
        hooks.insert(event.to_string(), handler.clone());
    }
    json!({ "hooks": hooks })
}

/// The session as its recorded hook events describe it, in the shape the
/// engine reads for any thread: a status, the latest run (one per prompt)
/// and whether a request is pending.
pub fn claude_state(session: &str, observed_at: Instant) -> ThreadState {
    let tid = claude_thread_id(session);
    let (mut turn, mut status, mut pending, mut events) = (0u64, None::<&str>, false, 0usize);
    for line in read_log_tail(&claude_log(session)).lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        events += 1;
        let event = v.get("event").and_then(Value::as_str).unwrap_or("");
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        match event {
            "UserPromptSubmit" => {
                turn += 1;
                status = Some("running");
                pending = false;
            }
            "Stop" => {
                status = Some("completed");
                pending = false;
            }
            "StopFailure" => {
                status = Some("failed");
                pending = false;
            }
            "PermissionRequest" => pending = true,
            "Notification" if NEEDS_INPUT.contains(&kind) => pending = true,
            "Notification" if kind == "idle_prompt" && status.is_none() => status = Some("completed"),
            e if ANSWERED.contains(&e) => pending = false,
            "SessionEnd" => {
                if status == Some("running") {
                    status = Some("cancelled");
                }
                pending = false;
            }
            _ => {}
        }
    }
    let mut thread = json!({
        "threadId": tid,
        "status": status,
        "pendingRequestCount": u64::from(pending),
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
