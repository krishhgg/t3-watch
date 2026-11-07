//! Regressions for the completion contract after the research incident
//! (a turn-mode watcher exited when parent run 2 finished, while four
//! delegated reviewers ran and the synthesis only came in run 6):
//!
//! - workstream mode settles only on T3's `resultRun` proof for this
//!   thread's newest run, never on a child's run or a stale proof;
//! - turn mode keeps its single-turn behavior and says when delegated work
//!   is still running;
//! - `--final-marker` as an explicit final-result contract;
//! - waiting, blocked, failed and cancelled are distinct outcomes;
//! - transport outages (tunnel 401s without a bearer challenge, 530/1033)
//!   are retried and never end a wait as success;
//! - `--follow`, `--after-run` and heartbeats.

mod common;

use common::*;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const P: &str = "mcp:launch:parent";
const C: &str = "mcp:launch:child";

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn run_id(thread: &str, n: u64) -> String {
    format!("run:thread:{thread}:ordinal:{n}")
}

fn mcp_env(mock: &MockMcp) -> Env {
    let env = Env::new();
    env.add_machine("mock", &mock.url, TOKEN);
    env
}

fn both(o: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn eventually(what: &str, timeout: Duration, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(ms(25));
    }
}

/// Every thread read so far asked for the header only: no message or
/// activity content (reading a delegated child's final result marks it
/// delivered in T3).
fn reads_were_header_only(mock: &MockMcp) -> bool {
    mock.with(|s| {
        s.read_args
            .iter()
            .all(|a| a["afterPosition"].as_u64() == Some(1_000_000_000) && a.get("itemId").is_none())
    })
}

fn events(hook: &MockHook) -> Vec<String> {
    hook.requests()
        .iter()
        .map(|r| r.headers.get("x-t3-watch-event").cloned().unwrap_or_default())
        .collect()
}

fn verify(hook: &HookReq, secret: &[u8]) -> bool {
    let h = &hook.headers;
    let msg = format!("{}.{}.", h["x-t3-watch-timestamp"], h["x-t3-watch-id"]);
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    let mut data = msg.into_bytes();
    data.extend_from_slice(&hook.body);
    let tag = ring::hmac::sign(&key, &data);
    h["x-t3-watch-signature"] == format!("sha256={}", hex(tag.as_ref()))
}

// ---- the research incident --------------------------------------------------

#[test]
fn workstream_waits_through_delegated_children_to_the_synthesis_run() {
    let mock = MockMcp::start();
    // Run 2 finished its own turn after launching four reviewers.
    mock.set_thread(P, "completed", 2);
    mock.with(|s| {
        s.delegated_pending.insert(P.into());
    });
    let env = mcp_env(&mock);
    let mut ws = env.spawn(&[
        "wait",
        "--no-daemon",
        "--until",
        "workstream",
        "--expect-min-run-ordinal",
        "2",
        "--slice",
        "200ms",
        "--timeout",
        "60s",
        P,
    ]);

    // What happened in the incident: a turn-mode watcher ends at run 2. It
    // now says the delegated work is still running.
    let turn = env.run(&[
        "wait",
        "--no-daemon",
        "--expect-min-run-ordinal",
        "2",
        "--timeout",
        "10s",
        P,
    ]);
    assert_eq!(turn.status.code(), Some(0), "{}", both(&turn));
    let t = output_json(&turn);
    assert_eq!(t["until"], "turn");
    assert_eq!(t["threads"][0]["delegatedWorkPending"], true, "{t}");
    assert!(t["threads"][0]["diagnostic"]
        .as_str()
        .unwrap()
        .contains("--until workstream"));

    still_running(&mut ws, ms(600));
    // Each reviewer's completion starts a parent run that delivers it.
    for ord in 3..=5 {
        mock.set_thread(P, "running", ord);
        still_running(&mut ws, ms(300));
        mock.set_thread(P, "completed", ord);
        still_running(&mut ws, ms(400));
    }
    // The last completion: synthesis run 6, then nothing is pending.
    mock.set_thread(P, "running", 6);
    still_running(&mut ws, ms(300));
    mock.with(|s| {
        s.delegated_pending.remove(P);
    });
    still_running(&mut ws, ms(400));
    mock.set_thread(P, "completed", 6);
    let (code, out, err) = finish(ws, secs(15));
    assert_eq!(code, 0, "stderr: {err}");
    let th = &out["threads"][0];
    assert_eq!(out["outcome"], "completed");
    assert_eq!(out["until"], "workstream");
    assert_eq!(th["runOrdinal"], 6);
    assert_eq!(th["resultRunId"], run_id(P, 6));
    assert_eq!(th["delegatedSettled"], true);
    assert!(
        reads_were_header_only(&mock),
        "the watcher must never read message content"
    );
}

#[test]
fn a_late_child_completion_after_the_proof_is_waited_on() {
    let mock = MockMcp::start();
    mock.set_thread(P, "completed", 4);
    // T3 proves run 4, but a late child completion starts run 5 before the
    // read.
    mock.with(|s| s.advance_after_wait = Some((P.into(), "running".into(), 5, false)));
    let env = mcp_env(&mock);
    let mut child = env.spawn(&["wait", "--no-daemon", "--until", "workstream", "--slice", "300ms", P]);
    still_running(&mut child, ms(1200));
    assert!(mock.calls("t3_thread_wait") >= 2, "the continuation must be waited on");
    mock.set_thread(P, "completed", 5);
    let (code, out, err) = finish(child, secs(10));
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out["threads"][0]["resultRunId"], run_id(P, 5));
    assert_eq!(out["threads"][0]["runOrdinal"], 5);
}

#[test]
fn an_earlier_result_run_settles_once_a_wait_covers_the_latest_run() {
    // T3 may name an earlier run as holding the final answer. The first
    // wait was sent before the watcher had seen run 5, so it proves nothing
    // about run 5; the next one, sent after, does.
    let mock = MockMcp::start();
    mock.set_thread(P, "completed", 5);
    mock.with(|s| {
        s.wait_override.insert(
            "resultRun".into(),
            json!({"runId": run_id(P, 4), "status": "completed"}),
        );
    });
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--no-daemon", "--until", "workstream", "--timeout", "10s", P]);
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
    let v = output_json(&out);
    assert_eq!(v["threads"][0]["resultRunId"], run_id(P, 4));
    assert_eq!(v["threads"][0]["runOrdinal"], 5);
    assert_eq!(
        mock.calls("t3_thread_wait"),
        2,
        "settles on the second wait, not the first"
    );
}

#[test]
fn a_childs_run_as_result_run_never_settles_the_parent() {
    let mock = MockMcp::start();
    mock.set_thread(P, "completed", 2);
    mock.with(|s| {
        s.wait_override.insert(
            "resultRun".into(),
            json!({"runId": "run:thread:mcp%3Alaunch%3Achild:ordinal:9", "status": "completed"}),
        );
    });
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--no-daemon", "--until", "workstream", "--timeout", "1500ms", P]);
    assert_eq!(out.status.code(), Some(3), "{}", both(&out));
    let v = output_json(&out);
    assert_eq!(v["threads"][0]["delegatedSettled"], false);
    assert!(v["threads"][0]["diagnostic"]
        .as_str()
        .unwrap()
        .contains("not a run of this thread"));
}

#[test]
fn an_ordinary_single_turn_is_unchanged() {
    let mock = MockMcp::start();
    mock.set_thread(P, "running", 1);
    let env = mcp_env(&mock);
    let mut child = env.spawn(&["wait", "--no-daemon", P]);
    still_running(&mut child, ms(300));
    mock.set_thread(P, "completed", 1);
    let (code, out, err) = finish(child, secs(10));
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out["outcome"], "completed");
    assert_eq!(out["threads"][0]["delegatedWorkPending"], false);
    assert!(out["threads"][0].get("diagnostic").is_none(), "{out}");
    assert_eq!(mock.calls("t3_thread_wait"), 1, "one long-poll");
    assert_eq!(mock.with(|s| s.probes), 1, "one delegated-work probe");
    assert!(reads_were_header_only(&mock));
    for k in ["startedAt", "endedAt", "watchId"] {
        assert!(out[k].is_string(), "{k}: {out}");
    }
}

// ---- --final-marker ---------------------------------------------------------

fn message(pos: u64, run: u64, text: &str) -> Value {
    json!({ "position": pos, "itemId": format!("msg-{pos}"), "type": "assistant_message",
            "status": "completed", "runId": run_id(P, run), "text": text })
}

#[test]
fn the_final_marker_must_be_in_the_final_message_of_the_deciding_run() {
    let mock = MockMcp::start();
    mock.set_thread(P, "completed", 6);
    mock.with(|s| {
        s.messages.insert(
            P.into(),
            vec![
                message(3, 5, "SYNTHESIS-DONE (but from run 5)"),
                message(9, 6, "interim: reviewers still running"),
            ],
        );
    });
    let env = mcp_env(&mock);
    let args = [
        "wait",
        "--no-daemon",
        "--until",
        "workstream",
        "--final-marker",
        "SYNTHESIS-DONE",
        "--timeout",
        "1500ms",
        P,
    ];
    let out = env.run(&args);
    assert_eq!(out.status.code(), Some(3), "no marker yet: {}", both(&out));
    let v = output_json(&out);
    assert_eq!(v["threads"][0]["finalMarkerSeen"], false);
    assert!(v["threads"][0]["diagnostic"].as_str().unwrap().contains("final marker"));

    // A long final message with the marker straddling a 50 000-character
    // page boundary still counts.
    let text = format!("{}SYNTHESIS-DONE tail", "y".repeat(49_995));
    mock.with(|s| s.messages.get_mut(P).unwrap().push(message(12, 6, &text)));
    let out = env.run(&args);
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
    assert_eq!(output_json(&out)["threads"][0]["finalMarkerSeen"], true);
}

#[test]
fn the_final_marker_is_never_read_from_a_delegated_child() {
    let mock = MockMcp::start();
    mock.set_thread(C, "completed", 1);
    mock.with(|s| {
        s.parent.insert(C.into(), P.into());
    });
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--no-daemon", "--final-marker", "DONE", "--timeout", "5s", C]);
    assert_eq!(out.status.code(), Some(2), "{}", both(&out));
    let v = output_json(&out);
    assert_eq!(v["error"]["kind"], "unsupported");
    assert!(v["error"]["message"].as_str().unwrap().contains("delivered"));
    assert!(
        reads_were_header_only(&mock),
        "the child's final result must stay unread"
    );
}

#[test]
fn local_final_marker_uses_the_last_assistant_message() {
    let env = Env::new();
    let t3 = MockT3::new(&env);
    t3.set(P, "completed", 2);
    let args = [
        "wait",
        "--no-daemon",
        "--transport",
        "local",
        "--t3-bin",
        t3.bin_str(),
        "--final-marker",
        "ALL-DONE",
        "--timeout",
        "1s",
        "--interval",
        "100ms",
        P,
    ];
    let out = env.run(&args);
    assert_eq!(out.status.code(), Some(3), "{}", both(&out));
    t3.set_messages(
        P,
        &json!([{"role": "assistant", "runId": run_id(P, 2), "text": "report ... ALL-DONE"}]),
    );
    let out = env.run(&args);
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
    assert_eq!(output_json(&out)["threads"][0]["finalMarkerSeen"], true);
}

// ---- needs input -------------------------------------------------------------

fn question_item(status: &str) -> Value {
    let qs = json!([{
        "id": "q1",
        "header": "Deploy",
        "question": "Use key sk-live-ABCDEFGHIJKLMNOPQRSTUV0123 for staging?",
        "options": [{"label": "Yes (Recommended)", "description": "uses the staging key"}, {"label": "No"}],
        "multiSelect": false
    }]);
    json!({ "position": 1, "type": "user_input_request", "status": status,
            "itemId": "turn-item:runtime-request:runtime-request%3Aabc", "text": qs.to_string() })
}

fn blocked(mock: &MockMcp, items: Vec<Value>, listed: &[&str]) {
    mock.set_thread(P, "running", 3);
    mock.with(|s| {
        s.pending_count.insert(P.into(), 1);
        s.pending_ids
            .insert(P.into(), listed.iter().map(|s| s.to_string()).collect());
        s.activity.insert(P.into(), items);
    });
}

#[test]
fn a_question_while_running_wakes_with_needs_input() {
    let mock = MockMcp::start();
    blocked(&mock, vec![question_item("pending")], &["runtime-request:abc"]);
    let env = mcp_env(&mock);
    let started = Instant::now();
    // In workstream mode too: a blocked thread will not finish by itself.
    let out = env.run(&[
        "wait",
        "--no-daemon",
        "--until",
        "workstream",
        "--slice",
        "300ms",
        "--expect-min-run-ordinal",
        "9",
        "--timeout",
        "20s",
        P,
    ]);
    assert_eq!(out.status.code(), Some(5), "{}", both(&out));
    assert!(started.elapsed() < secs(5), "must wake within a slice or two");
    let v = output_json(&out);
    assert_eq!(v["event"], "needs_input");
    assert_eq!(v["outcome"], "needs_input");
    let t = &v["threads"][0];
    assert_eq!(t["status"], "running");
    assert_eq!(t["outcome"], "needs_input");
    assert_eq!(t["pendingRequestCount"], 1);
    let r = &t["requests"][0];
    assert_eq!(r["kind"], "question");
    assert_eq!(r["requestId"], "runtime-request:abc");
    assert_eq!(r["userOnly"], false);
    assert_eq!(r["questions"][0]["header"], "Deploy");
    assert_eq!(r["questions"][0]["options"], json!(["Yes (Recommended)", "No"]));
    let text = v.to_string();
    assert!(!text.contains("sk-live-ABCDEF"), "credential leaked: {text}");
    assert!(t["pendingKey"].as_str().unwrap().starts_with("pk-"));
    assert!(v["action"].as_str().unwrap().contains("--acknowledge"));
    // Read-only: only `list`, never `respond`.
    let actions: Vec<Value> = mock.with(|s| s.pending_args.iter().map(|a| a["action"].clone()).collect());
    assert!(
        !actions.is_empty() && actions.iter().all(|a| a == "list"),
        "{actions:?}"
    );
}

#[test]
fn an_approval_is_needs_input_for_the_user_only() {
    let mock = MockMcp::start();
    // Approvals are not listed by t3_pending_request.
    let approval = json!({ "position": 1, "type": "command_approval_request", "status": "pending",
                           "itemId": "turn-item:runtime-request:ap-1", "text": "Run: rm -rf build (password=hunter2)" });
    blocked(&mock, vec![approval], &[]);
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--no-daemon", "--slice", "300ms", "--timeout", "20s", P]);
    assert_eq!(out.status.code(), Some(5), "{}", both(&out));
    let v = output_json(&out);
    let r = &v["threads"][0]["requests"][0];
    assert_eq!(r["kind"], "approval");
    assert_eq!(r["userOnly"], true);
    assert_eq!(r["requestId"], "ap-1");
    assert!(!v.to_string().contains("hunter2"));
}

#[test]
fn an_acknowledged_request_keeps_waiting_and_a_new_one_wakes() {
    let mock = MockMcp::start();
    blocked(&mock, vec![question_item("pending")], &["runtime-request:abc"]);
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--no-daemon", "--slice", "300ms", "--timeout", "20s", P]);
    assert_eq!(out.status.code(), Some(5));
    let key = output_json(&out)["threads"][0]["pendingKey"]
        .as_str()
        .unwrap()
        .to_string();

    let mut child = env.spawn(&["wait", "--no-daemon", "--slice", "300ms", "--acknowledge", &key, P]);
    still_running(&mut child, ms(1200));
    // Someone answers; the run finishes.
    mock.with(|s| {
        s.pending_count.insert(P.into(), 0);
        s.pending_ids.insert(P.into(), vec![]);
        s.activity.insert(P.into(), vec![question_item("completed")]);
    });
    mock.set_thread(P, "completed", 3);
    let (code, out, err) = finish(child, secs(10));
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out["outcome"], "completed");

    // A second, different request is not covered by the old key.
    mock.set_thread(P, "running", 4);
    let mut second = question_item("pending");
    second["itemId"] = json!("turn-item:runtime-request:runtime-request%3Adef");
    blocked(&mock, vec![second], &["runtime-request:def"]);
    let out = env.run(&[
        "wait",
        "--no-daemon",
        "--slice",
        "300ms",
        "--acknowledge",
        &key,
        "--timeout",
        "20s",
        P,
    ]);
    assert_eq!(out.status.code(), Some(5), "{}", both(&out));
}

#[test]
fn failed_interrupted_and_cancelled_runs_are_not_success() {
    for status in ["failed", "interrupted", "cancelled", "error"] {
        let mock = MockMcp::start();
        mock.set_thread(P, status, 2);
        let env = mcp_env(&mock);
        for mode in ["turn", "workstream"] {
            let out = env.run(&["wait", "--no-daemon", "--until", mode, "--timeout", "10s", P]);
            assert_eq!(out.status.code(), Some(6), "{status} {mode}: {}", both(&out));
            let v = output_json(&out);
            assert_eq!(v["event"], "settled");
            assert_eq!(v["outcome"], "failed");
            assert_eq!(v["threads"][0]["outcome"], "failed");
        }
    }
}

#[test]
fn a_failed_parent_turn_with_delegated_work_waits_for_the_workstream() {
    let mock = MockMcp::start();
    mock.set_thread(P, "interrupted", 2);
    mock.with(|s| {
        s.delegated_pending.insert(P.into());
    });
    let env = mcp_env(&mock);
    let mut child = env.spawn(&["wait", "--no-daemon", "--until", "workstream", "--slice", "200ms", P]);
    still_running(&mut child, ms(800));
    mock.set_thread(P, "completed", 3);
    mock.with(|s| {
        s.delegated_pending.remove(P);
    });
    let (code, out, err) = finish(child, secs(10));
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out["threads"][0]["resultRunId"], run_id(P, 3));
}

// ---- transport ---------------------------------------------------------------

#[test]
fn a_tunnel_401_without_a_bearer_challenge_is_an_outage_not_an_auth_failure() {
    let mock = MockMcp::start();
    mock.set_thread(P, "completed", 2);
    mock.with(|s| {
        s.fail_queue.push_back((401, "Unauthorized: Tunnel not found".into()));
        s.fail_queue.push_back((530, "error code: 1033".into()));
    });
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--no-daemon", "--until", "workstream", "--timeout", "30s", P]);
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
    let v = output_json(&out);
    assert_eq!(v["outcome"], "completed");
    assert_eq!(mock.calls("failed:401"), 1);
    assert_eq!(mock.calls("failed:530"), 1);
    assert_eq!(mock.calls("refresh"), 0, "no token recovery for a tunnel outage");
}

#[test]
fn an_outage_never_completes_a_wait_and_is_reported_both_ways() {
    let mock = MockMcp::start();
    mock.set_thread(P, "completed", 2);
    for _ in 0..2 {
        mock.with(|s| s.fail_queue.push_back((530, "error code: 1033".into())));
    }
    let hook = MockHook::start();
    let env = mcp_env(&mock);
    let out = env.run(&[
        "wait",
        "--no-daemon",
        "--until",
        "workstream",
        "--notify-unreachable",
        "0s",
        "--webhook-url",
        &hook.url,
        "--timeout",
        "30s",
        P,
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
    assert_eq!(events(&hook), ["upstream_unreachable", "upstream_restored", "settled"]);
    let unreachable: Value = serde_json::from_slice(&hook.requests()[0].body).unwrap();
    assert_eq!(unreachable["upstream"]["reachable"], false);
    assert!(unreachable["upstream"]["lastError"].as_str().unwrap().contains("1033"));
    let ids: Vec<String> = hook
        .requests()
        .iter()
        .map(|r| r.headers["x-t3-watch-id"].clone())
        .collect();
    assert_eq!(
        ids.iter().collect::<std::collections::HashSet<_>>().len(),
        3,
        "one id per event"
    );
    for r in hook.requests() {
        assert_eq!(r.headers["idempotency-key"], r.headers["x-t3-watch-id"]);
    }
}

#[test]
fn an_outage_until_the_timeout_reports_where_it_stood() {
    let mock = MockMcp::start();
    mock.set_thread(P, "completed", 2);
    for _ in 0..50 {
        mock.with(|s| s.fail_queue.push_back((502, "bad gateway".into())));
    }
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--no-daemon", "--until", "workstream", "--timeout", "2s", P]);
    assert_eq!(out.status.code(), Some(3), "{}", both(&out));
    let v = output_json(&out);
    assert_eq!(v["outcome"], "timeout");
    let up = &v["threads"][0]["upstream"];
    assert_eq!(up["reachable"], false);
    assert!(up["consecutiveFailures"].as_u64().unwrap() >= 1);
    assert!(up["unreachableSince"].as_str().unwrap().ends_with('Z'));
}

// ---- follow, after-run, heartbeat ---------------------------------------------

#[test]
fn follow_reports_a_failed_run_and_waits_for_the_resend() {
    let mock = MockMcp::start();
    mock.set_thread(P, "failed", 5);
    let hook = MockHook::start();
    let env = mcp_env(&mock);
    let mut child = env.spawn(&[
        "wait",
        "--no-daemon",
        "--follow",
        "--expect-min-run-ordinal",
        "5",
        "--webhook-url",
        &hook.url,
        P,
    ]);
    eventually("the run_failed progress webhook", secs(10), || {
        events(&hook) == ["run_failed"]
    });
    still_running(&mut child, ms(500));
    // The resend becomes run 6.
    mock.set_thread(P, "running", 6);
    still_running(&mut child, ms(500));
    mock.set_thread(P, "completed", 6);
    let (code, out, err) = finish(child, secs(15));
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out["threads"][0]["runOrdinal"], 6);
    assert_eq!(out["failedRuns"][0]["runId"], run_id(P, 5));
    assert_eq!(events(&hook), ["run_failed", "settled"]);
    assert!(err.contains("progress"), "progress is logged to stderr: {err}");
}

#[test]
fn after_run_ignores_that_run_and_checks_its_thread() {
    let mock = MockMcp::start();
    mock.set_thread(P, "failed", 5);
    let env = mcp_env(&mock);
    let failed_run = run_id(P, 5);
    let out = env.run(&[
        "wait",
        "--no-daemon",
        "--after-run",
        &failed_run,
        "--timeout",
        "1500ms",
        P,
    ]);
    assert_eq!(out.status.code(), Some(3), "run 5 is ignored: {}", both(&out));
    mock.set_thread(P, "completed", 6);
    let out = env.run(&["wait", "--no-daemon", "--after-run", &failed_run, "--timeout", "10s", P]);
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
    let other = run_id(C, 5);
    let out = env.run(&["wait", "--no-daemon", "--after-run", &other, P]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(output_json(&out)["error"]["kind"], "usage");
}

#[test]
fn heartbeats_are_signed_and_share_the_watch_id_with_the_result() {
    let mock = MockMcp::start();
    mock.set_thread(P, "running", 1);
    let hook = MockHook::start();
    let env = mcp_env(&mock);
    let child = env
        .cmd(&[
            "wait",
            "--no-daemon",
            "--heartbeat",
            "1s",
            "--webhook-url",
            &hook.url,
            "--webhook-secret-env",
            "HB_SECRET",
            P,
        ])
        .env("HB_SECRET", "hb-secret")
        .spawn()
        .unwrap();
    eventually("two heartbeats", secs(10), || {
        events(&hook).iter().filter(|e| *e == "heartbeat").count() >= 2
    });
    mock.set_thread(P, "completed", 1);
    let (code, out, err) = finish(child, secs(10));
    assert_eq!(code, 0, "stderr: {err}");
    let reqs = hook.requests();
    let beats: Vec<Value> = reqs
        .iter()
        .filter(|r| r.headers["x-t3-watch-event"] == "heartbeat")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(beats[0]["seq"], 0);
    assert_eq!(beats[1]["seq"], 1);
    assert!(beats.iter().all(|b| b["watchId"] == out["watchId"]));
    assert!(beats[0]["note"].as_str().unwrap().contains("liveness"));
    assert_eq!(beats[0]["threadIds"], json!([P]));
    assert!(
        beats[0].get("threads").is_none(),
        "results own `threads`; heartbeats name ids only"
    );
    assert!(reqs.iter().all(|r| verify(r, b"hb-secret")));
    assert_eq!(reqs.last().unwrap().headers["x-t3-watch-event"], "settled");
}
