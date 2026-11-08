//! Native Claude Code sessions (experimental): `t3-watch hook claude-code`
//! records the documented hook events, and `wait claude-code:<session_id>`
//! watches them through the same engine as T3 threads. The hook input here
//! is synthetic, shaped as the Claude Code hooks reference documents it.

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::process::Stdio;
use std::time::Duration;

const S: &str = "00893aaf-19fa-41d2-8238-13269b9b3ca0";

fn target() -> String {
    format!("claude-code:{S}")
}

/// Runs the hook as Claude Code would: the JSON on stdin. Returns its exit
/// code and stdout.
fn hook(env: &Env, event: Value) -> (Option<i32>, String) {
    let mut child = env.cmd(&["hook", "claude-code"]).stdin(Stdio::piped()).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(event.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned())
}

fn event(name: &str, extra: Value) -> Value {
    let mut v = json!({
        "session_id": S,
        "transcript_path": "/home/user/.claude/projects/p/00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
        "cwd": "/home/user/p",
        "permission_mode": "default",
        "hook_event_name": name,
    });
    if let (Value::Object(v), Value::Object(e)) = (&mut v, extra) {
        v.extend(e);
    }
    v
}

fn log(env: &Env) -> String {
    fs::read_to_string(env.home.join("native").join("claude-code").join(format!("{S}.jsonl"))).unwrap_or_default()
}

#[test]
fn the_hook_records_names_only_never_prints_and_never_fails() {
    let env = Env::new();
    let (code, out) = hook(
        &env,
        event(
            "UserPromptSubmit",
            json!({ "prompt": "PROMPT_CANARY please", "prompt_id": "p-1" }),
        ),
    );
    assert_eq!(code, Some(0));
    assert_eq!(out, "", "an async hook must print no decision");
    let (code, out) = hook(
        &env,
        event(
            "PreToolUse",
            json!({ "tool_name": "Bash", "tool_input": { "command": "echo TOOL_CANARY" } }),
        ),
    );
    assert_eq!((code, out.as_str()), (Some(0), ""));
    let (code, _) = hook(
        &env,
        event(
            "Stop",
            json!({ "last_assistant_message": "MESSAGE_CANARY", "stop_hook_active": false }),
        ),
    );
    assert_eq!(code, Some(0));
    let text = log(&env);
    for canary in [
        "PROMPT_CANARY",
        "TOOL_CANARY",
        "MESSAGE_CANARY",
        "transcript",
        "/home/user",
    ] {
        assert!(!text.contains(canary), "{canary} reached the log: {text}");
    }
    assert_eq!(text.lines().count(), 3, "{text}");
    let path = env.home.join("native").join("claude-code").join(format!("{S}.jsonl"));
    assert_eq!(mode(&path), 0o600);
    // Whatever arrives, the hook succeeds and prints nothing.
    for bad in [
        json!("not an object"),
        json!({ "session_id": "../escape", "hook_event_name": "Stop" }),
        json!({}),
    ] {
        let (code, out) = hook(&env, bad.clone());
        assert_eq!((code, out.as_str()), (Some(0), ""), "{bad}");
    }
    assert!(!env.home.join("native").join("escape.jsonl").exists());
    assert_eq!(log(&env).lines().count(), 3);
}

#[test]
fn a_turn_is_watched_from_its_hook_events_with_and_without_the_daemon() {
    for daemon in [true, false] {
        let env = Env::new();
        let hook_rx = MockHook::start();
        hook(&env, event("SessionStart", json!({ "source": "startup" })));
        hook(&env, event("UserPromptSubmit", json!({ "prompt": "x" })));
        let mut args = vec!["wait", "--timeout", "20s", "--webhook-url", &hook_rx.url];
        if !daemon {
            args.push("--no-daemon");
        }
        let t = target();
        args.push(&t);
        let mut child = env.spawn(&args);
        still_running(&mut child, Duration::from_millis(1200));
        hook(&env, event("Stop", json!({ "last_assistant_message": "done" })));
        let (code, out, err) = finish(child, Duration::from_secs(10));
        assert_eq!(code, 0, "daemon={daemon}: {out} {err}");
        assert_eq!(out["outcome"], "completed", "{out}");
        let th = &out["threads"][0];
        assert_eq!(th["threadId"], t, "{out}");
        assert_eq!(th["runOrdinal"], 1, "{out}");
        let reqs = hook_rx.requests();
        assert_eq!(reqs.len(), 1, "daemon={daemon}");
        assert_eq!(reqs[0].headers["x-t3-watch-event"], "settled");
        assert!(String::from_utf8_lossy(&reqs[0].body).contains(&t));
    }
}

#[test]
fn a_permission_prompt_is_needs_input_until_answered() {
    let env = Env::new();
    hook(&env, event("UserPromptSubmit", json!({ "prompt": "x" })));
    hook(
        &env,
        event(
            "Notification",
            json!({ "message": "Claude needs your permission", "notification_type": "permission_prompt" }),
        ),
    );
    let out = env.run(&["wait", "--no-daemon", "--timeout", "5s", &target()]);
    assert_eq!(out.status.code(), Some(5), "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(output_json(&out)["event"], "needs_input");
    // Answered by its user (never by t3-watch): the tool ran.
    hook(&env, event("PostToolUse", json!({ "tool_name": "Bash" })));
    let out = env.run(&["wait", "--no-daemon", "--timeout", "1500ms", &target()]);
    assert_eq!(out.status.code(), Some(3), "running again, not needs-input");
    hook(&env, event("PermissionRequest", json!({ "tool_name": "Write" })));
    let out = env.run(&["wait", "--no-daemon", "--timeout", "5s", &target()]);
    assert_eq!(out.status.code(), Some(5));
    hook(&env, event("Stop", json!({})));
    let out = env.run(&["wait", "--no-daemon", "--timeout", "5s", &target()]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
}

#[test]
fn a_failed_turn_and_a_session_ended_mid_turn_are_failures() {
    let env = Env::new();
    hook(&env, event("UserPromptSubmit", json!({ "prompt": "x" })));
    hook(&env, event("StopFailure", json!({})));
    let out = env.run(&["wait", "--no-daemon", "--timeout", "5s", &target()]);
    assert_eq!(out.status.code(), Some(6));
    assert_eq!(output_json(&out)["outcome"], "failed");

    hook(&env, event("UserPromptSubmit", json!({ "prompt": "y" })));
    hook(&env, event("SessionEnd", json!({ "reason": "prompt_input_exit" })));
    let out = env.run(&["wait", "--no-daemon", "--timeout", "5s", &target()]);
    assert_eq!(out.status.code(), Some(6));
    let v = output_json(&out);
    assert_eq!(v["threads"][0]["status"], "cancelled", "{v}");
    assert_eq!(v["threads"][0]["runOrdinal"], 2, "{v}");
}

#[test]
fn a_follow_up_turn_is_awaited_with_its_ordinal() {
    // A bot reusing a session waits for the turn after the one it saw.
    let env = Env::new();
    hook(&env, event("UserPromptSubmit", json!({ "prompt": "x" })));
    hook(&env, event("Stop", json!({})));
    let t = target();
    let mut child = env.spawn(&["wait", "--timeout", "20s", "--expect-min-run-ordinal", "2", &t]);
    still_running(&mut child, Duration::from_millis(1200));
    hook(&env, event("UserPromptSubmit", json!({ "prompt": "y" })));
    still_running(&mut child, Duration::from_millis(800));
    hook(&env, event("Stop", json!({})));
    let (code, out, _) = finish(child, Duration::from_secs(10));
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["threads"][0]["runOrdinal"], 2, "{out}");
}

#[test]
fn a_session_with_no_events_yet_keeps_waiting_and_says_why() {
    let env = Env::new();
    let out = env.run(&["wait", "--no-daemon", "--timeout", "1500ms", &target()]);
    assert_eq!(out.status.code(), Some(3));
    let v = output_json(&out);
    assert!(v.to_string().contains("no hook events recorded yet"), "{v}");
    // Idle with nothing seen before: settled.
    hook(
        &env,
        event("Notification", json!({ "notification_type": "idle_prompt" })),
    );
    let out = env.run(&["wait", "--no-daemon", "--timeout", "5s", &target()]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn t3_contracts_and_codex_targets_are_refused() {
    let env = Env::new();
    let out = env.run(&["wait", "--until", "workstream", &target()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(output_json(&out).to_string().contains("T3 contracts"));
    let out = env.run(&["wait", "--final-marker", "DONE", &target()]);
    assert_eq!(out.status.code(), Some(2));
    let out = env.run(&["wait", "codex://threads/01a1b891-e945-7f70-b755-db5ece6dcaff"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(output_json(&out).to_string().contains("not supported yet"));
    for bad in ["claude-code:", "claude-code:../x", "claude-code:a b"] {
        let out = env.run(&["wait", bad]);
        assert_eq!(out.status.code(), Some(2), "{bad}");
    }
}

#[test]
fn print_settings_gives_async_hooks_and_touches_no_settings_file() {
    let env = Env::new();
    let out = env.run(&["hook", "claude-code", "--print-settings"]);
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    for e in [
        "UserPromptSubmit",
        "Stop",
        "StopFailure",
        "Notification",
        "PermissionRequest",
        "SessionEnd",
    ] {
        let h = &v["hooks"][e][0]["hooks"][0];
        assert_eq!(h["async"], true, "{e}: {v}");
        assert!(h["command"].as_str().unwrap().ends_with("hook claude-code"), "{e}");
    }
}
