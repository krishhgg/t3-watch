//! Regressions for the third review (Sol, ordinal 3) of the completion
//! contract:
//!
//! - P1: a later read of the same cycle (the marker listing, a message
//!   page, the re-validation header) that shows a newer or running parent
//!   voids the observation; completion needs a fresh proof.
//! - P1: the outcome of a proven workstream is the result run's own status;
//!   a failed, interrupted or cancelled `resultRun` is never success, and a
//!   proof that disagrees with the header is re-checked.
//! - P2: a thread already blocked on input is reported before the first
//!   long-poll, within a short timeout.
//! - P2: a delegated child's timeline is never read, so its historical final
//!   results are never marked delivered.

mod common;

use common::*;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const P: &str = "mcp:launch:parent";
const C: &str = "mcp:launch:child";

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

fn message(pos: u64, run: u64, text: &str) -> Value {
    json!({ "position": pos, "itemId": format!("msg-{pos}"), "type": "assistant_message",
            "status": "completed", "runId": run_id(P, run), "text": text })
}

fn events(hook: &MockHook) -> Vec<(String, Value)> {
    hook.requests()
        .iter()
        .map(|r| {
            (
                r.headers.get("x-t3-watch-event").cloned().unwrap_or_default(),
                serde_json::from_slice(&r.body).unwrap_or(Value::Null),
            )
        })
        .collect()
}

// ---- P1: marker reads that reveal a newer running parent -------------------

/// Sol's interleaving: the wait proves run 2, the header shows run 2
/// completed, then the thread moves to run 3 (running) right after read
/// number `after`. Run 2's final message has FINAL-DONE. The watcher must
/// not report completion for run 2; it completes only once run 3 has its own
/// proof and marker.
fn marker_drift(after: usize) {
    let mock = MockMcp::start();
    mock.set_thread(P, "completed", 2);
    mock.with(|s| {
        s.messages
            .insert(P.into(), vec![message(5, 2, "summary ... FINAL-DONE")]);
        // Read 1: the header before the first wait. Read 2: the header after
        // it. Then, by `after`: the marker listing (3), the message page (4),
        // the header re-validation (5).
        s.after_read.insert(after, (P.into(), "running".into(), 3));
    });
    let hook = MockHook::start();
    let env = mcp_env(&mock);
    let mut child = env.spawn(&[
        "wait",
        "--no-daemon",
        "--until",
        "workstream",
        "--final-marker",
        "FINAL-DONE",
        "--slice",
        "300ms",
        "--webhook-url",
        &hook.url,
        P,
    ]);
    still_running(&mut child, Duration::from_millis(1500));
    assert!(hook.requests().is_empty(), "nothing may be announced for run 2");

    // Run 3 finishes with its own final message.
    mock.with(|s| {
        s.messages
            .get_mut(P)
            .unwrap()
            .push(message(9, 3, "synthesis FINAL-DONE"))
    });
    mock.set_thread(P, "completed", 3);
    let (code, out, err) = finish(child, Duration::from_secs(15));
    assert_eq!(code, 0, "after {after}: {err}");
    let t = &out["threads"][0];
    assert_eq!(t["runOrdinal"], 3, "{out}");
    assert_eq!(t["resultRunId"], run_id(P, 3));
    assert_eq!(t["finalMarkerSeen"], true);
    let ev = events(&hook);
    assert_eq!(ev.len(), 1, "exactly one settled event: {ev:?}");
    assert_eq!(ev[0].0, "settled");
    assert_eq!(ev[0].1["threads"][0]["runOrdinal"], 3);
}

#[test]
fn a_running_parent_seen_by_the_marker_listing_voids_completion() {
    marker_drift(2);
}

#[test]
fn a_running_parent_seen_by_the_message_page_voids_completion() {
    marker_drift(3);
}

#[test]
fn a_running_parent_seen_by_the_revalidation_read_voids_completion() {
    marker_drift(4);
}

#[test]
fn a_newer_run_seen_by_the_turn_probe_voids_completion() {
    // Turn mode: the header says run 1 completed, but the delegated-work
    // probe already names run 2.
    let mock = MockMcp::start();
    mock.set_thread(P, "completed", 1);
    mock.with(|s| s.after_read.insert(2, (P.into(), "running".into(), 2)));
    let env = mcp_env(&mock);
    let mut child = env.spawn(&["wait", "--no-daemon", "--slice", "300ms", P]);
    std::thread::sleep(Duration::from_millis(1200));
    assert!(
        child.try_wait().unwrap().is_none(),
        "run 1 must not be reported once run 2 is seen"
    );
    mock.set_thread(P, "completed", 2);
    let (code, out, err) = finish(child, Duration::from_secs(10));
    assert_eq!(code, 0, "{err}");
    assert_eq!(out["threads"][0]["runOrdinal"], 2);
}

// ---- P1: the deciding run's status decides --------------------------------

#[test]
fn a_failed_result_run_ahead_of_the_header_is_never_success() {
    // Sol's repro: resultRun is run 6 and failed; the header still shows
    // run 2 completed.
    for status in ["failed", "interrupted", "cancelled"] {
        let mock = MockMcp::start();
        mock.set_thread(P, "completed", 2);
        mock.with(|s| {
            s.wait_override
                .insert("resultRun".into(), json!({"runId": run_id(P, 6), "status": status}));
        });
        let env = mcp_env(&mock);
        let out = env.run(&["wait", "--no-daemon", "--until", "workstream", "--timeout", "1500ms", P]);
        assert_eq!(out.status.code(), Some(3), "{status}: {}", both(&out));
        let v = output_json(&out);
        assert_ne!(v["outcome"], "completed");
        assert_eq!(v["threads"][0]["settled"], false);
    }
}

#[test]
fn a_failed_earlier_result_run_is_a_failure() {
    // T3 names run 4 as holding the answer, and run 4 failed; the header
    // shows a later run 5 completed.
    for status in ["failed", "interrupted", "cancelled"] {
        let mock = MockMcp::start();
        mock.set_thread(P, "completed", 5);
        mock.with(|s| {
            s.wait_override
                .insert("resultRun".into(), json!({"runId": run_id(P, 4), "status": status}));
        });
        let env = mcp_env(&mock);
        let out = env.run(&["wait", "--no-daemon", "--until", "workstream", "--timeout", "10s", P]);
        assert_eq!(out.status.code(), Some(6), "{status}: {}", both(&out));
        let v = output_json(&out);
        assert_eq!(v["outcome"], "failed");
        let t = &v["threads"][0];
        assert_eq!(t["outcome"], "failed");
        assert_eq!(t["resultRunId"], run_id(P, 4));
        assert_eq!(t["resultRunStatus"], status);
    }
}

#[test]
fn a_proof_and_header_that_disagree_about_one_run_are_rechecked() {
    for status in ["failed", "interrupted", "cancelled"] {
        let mock = MockMcp::start();
        mock.set_thread(P, "completed", 5);
        mock.with(|s| {
            s.wait_override
                .insert("resultRun".into(), json!({"runId": run_id(P, 5), "status": status}));
        });
        let env = mcp_env(&mock);
        let out = env.run(&["wait", "--no-daemon", "--until", "workstream", "--timeout", "1500ms", P]);
        assert_eq!(out.status.code(), Some(3), "{status}: {}", both(&out));
        let v = output_json(&out);
        assert!(
            v["threads"][0]["diagnostic"].as_str().unwrap().contains("disagree")
                || v["threads"][0]["diagnostic"].as_str().unwrap().contains("re-checking")
        );
    }
    // Same run, both failed: a failure, exit 6.
    let mock = MockMcp::start();
    mock.set_thread(P, "interrupted", 5);
    mock.with(|s| {
        s.wait_override.insert(
            "resultRun".into(),
            json!({"runId": run_id(P, 5), "status": "interrupted"}),
        );
    });
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--no-daemon", "--until", "workstream", "--timeout", "10s", P]);
    assert_eq!(out.status.code(), Some(6), "{}", both(&out));
}

// ---- P2: input that is already pending -------------------------------------

#[test]
fn input_pending_before_the_first_long_poll_is_reported_at_once() {
    let mock = MockMcp::start();
    mock.set_thread(P, "running", 3);
    mock.with(|s| {
        s.pending_count.insert(P.into(), 1);
    });
    let hook = MockHook::start();
    let env = mcp_env(&mock);
    let started = Instant::now();
    // Sol's repro: a 900 ms timeout and a long slice.
    let out = env.run(&[
        "wait",
        "--no-daemon",
        "--timeout",
        "900ms",
        "--slice",
        "5m",
        "--webhook-url",
        &hook.url,
        P,
    ]);
    assert_eq!(out.status.code(), Some(5), "{}", both(&out));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(mock.calls("t3_thread_read") >= 1);
    assert_eq!(mock.calls("t3_thread_wait"), 0, "reported before any long-poll");
    assert_eq!(events(&hook)[0].0, "needs_input");
}

// ---- P2: a delegated child's timeline stays unread -------------------------

#[test]
fn a_running_child_with_pending_input_is_reported_without_reading_its_timeline() {
    let mock = MockMcp::start();
    mock.set_thread(C, "running", 2);
    let q = json!([{ "id": "q1", "header": "Scope", "question": "Review only the diff?",
                     "options": [{"label": "Yes"}, {"label": "No"}], "multiSelect": false }]);
    mock.with(|s| {
        s.parent.insert(C.into(), P.into());
        s.pending_count.insert(C.into(), 1);
        s.pending_ids.insert(C.into(), vec!["runtime-request:q1".into()]);
        s.pending_reads.insert(
            "runtime-request:q1".into(),
            json!({ "requestId": "runtime-request:q1", "questions": q }),
        );
        // Run 1's final result, already in the timeline, plus the request.
        s.activity.insert(
            C.into(),
            vec![
                json!({ "position": 0, "type": "assistant_message", "status": "completed",
                        "runId": run_id(C, 1), "itemId": "final-1", "text": "run 1 final result" }),
                json!({ "position": 1, "type": "user_input_request", "status": "pending",
                        "itemId": "turn-item:runtime-request:runtime-request%3Aq1", "text": q.to_string() }),
            ],
        );
        s.messages.insert(
            C.into(),
            vec![
                json!({ "position": 0, "type": "assistant_message", "status": "completed",
                         "runId": run_id(C, 1), "itemId": "final-1", "text": "run 1 final result" }),
            ],
        );
    });
    let env = mcp_env(&mock);
    for until in ["turn", "workstream"] {
        let out = env.run(&["wait", "--no-daemon", "--until", until, "--timeout", "10s", C]);
        assert_eq!(out.status.code(), Some(5), "{until}: {}", both(&out));
        let v = output_json(&out);
        let r = &v["threads"][0]["requests"][0];
        assert_eq!(r["requestId"], "runtime-request:q1");
        assert_eq!(r["questions"][0]["header"], "Scope");
        assert_eq!(v["threads"][0]["parentThreadId"], P);
    }
    mock.with(|s| {
        assert_eq!(s.child_final_reads, 0, "a child's final result was read");
        for a in &s.read_args {
            assert_eq!(a["afterPosition"], 1_000_000_000, "only header reads of a child: {a}");
            assert!(a.get("itemId").is_none() && a.get("view").is_none(), "{a}");
        }
        assert!(s
            .pending_args
            .iter()
            .all(|a| matches!(a["action"].as_str(), Some("list" | "read"))));
    });
}
