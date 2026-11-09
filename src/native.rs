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

/// What an open request waits for. Tool events never close one: with tool
/// inputs and ids deliberately not recorded, a later tool event (a parallel
/// call, an async hook arriving late) cannot be told apart from the one
/// that was waiting. A request stays open until its own kind is answered
/// (`PermissionDenied` for a permission, `ElicitationResult` for an
/// elicitation), one at a time, or its turn ends.
#[derive(Clone, Copy, PartialEq)]
enum Waits {
    Permission,
    Elicitation,
    Input,
}

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

/// A tool's name (`Bash`, `mcp__server__tool`): metadata, recorded so a
/// denial closes that tool's request. Never its input.
pub(crate) fn tool_name(v: &Value) -> Option<String> {
    v.get("tool_name")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty() && t.len() <= 128)
        .filter(|t| {
            t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.'))
        })
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
        "tool": tool_name(&v),
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
pub(crate) fn shell_quote(s: &str) -> String {
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
/// and its open requests, each with its own id.
pub fn claude_state(session: &str, observed_at: Instant) -> ThreadState {
    let tid = claude_thread_id(session);
    let (mut turn, mut status, mut events) = (0u64, None::<&str>, 0usize);
    // (id, what it waits for, tool) for each open request: independent, so
    // one answered request leaves the others open.
    let mut open: Vec<(String, Waits, Option<String>)> = Vec::new();
    for (n, line) in read_log_tail(&claude_log(session)).lines().enumerate() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        events += 1;
        let event = v.get("event").and_then(Value::as_str).unwrap_or("");
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let tool = v.get("tool").and_then(Value::as_str).map(str::to_string);
        // A subagent's stop or end is not the session's.
        let subagent = v.get("subagent").and_then(Value::as_bool) == Some(true);
        let id = format!("cc-{turn}-{n}");
        match event {
            "UserPromptSubmit" => {
                turn += 1;
                status = Some("running");
                open.clear();
            }
            "Stop" | "StopFailure" | "SessionEnd" if subagent => {}
            "Stop" => {
                status = Some("completed");
                open.clear();
            }
            "StopFailure" => {
                status = Some("failed");
                open.clear();
            }
            "SessionEnd" => {
                if status == Some("running") {
                    status = Some("cancelled");
                }
                open.clear();
            }
            "PermissionRequest" => open.push((id, Waits::Permission, tool)),
            // The dialog's own notification: the same request as an open
            // PermissionRequest, a new one only if none is open.
            "Notification" if kind == "permission_prompt" => {
                if !open.iter().any(|(_, w, _)| *w == Waits::Permission) {
                    open.push((id, Waits::Permission, None));
                }
            }
            "Notification" if kind == "elicitation_dialog" || kind == "elicitation_url_dialog" => {
                open.push((id, Waits::Elicitation, None))
            }
            "Notification" if kind == "agent_needs_input" => open.push((id, Waits::Input, None)),
            "PermissionDenied" => {
                // That tool's oldest open permission request, else the
                // oldest one.
                let at = open
                    .iter()
                    .position(|(_, w, t)| *w == Waits::Permission && tool.is_some() && *t == tool)
                    .or_else(|| open.iter().position(|(_, w, _)| *w == Waits::Permission));
                if let Some(at) = at {
                    open.remove(at);
                }
            }
            "ElicitationResult" => {
                if let Some(at) = open.iter().position(|(_, w, _)| *w == Waits::Elicitation) {
                    open.remove(at);
                }
            }
            // No turn recorded yet: idle proves nothing about one, so it
            // never counts as a completion.
            _ => {}
        }
    }
    let mut thread = json!({
        "threadId": tid,
        "status": status,
        "pendingRequestCount": open.len(),
        "title": if events == 0 {
            "Claude Code session (no hook events recorded yet)"
        } else if turn == 0 {
            "Claude Code session (no turn recorded yet)"
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
    let mut state = ThreadState::from_thread_json(&json!({ "thread": thread }), &tid, observed_at);
    state.pending_ids = open.into_iter().map(|(id, _, _)| id).collect();
    state
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
