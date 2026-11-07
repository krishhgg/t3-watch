//! Regressions for the third review (Sol, ordinal 3) of event delivery:
//!
//! - P1: with stdout and stderr both closed (a harness that went away),
//!   every event is still persisted and delivered and the exit code is
//!   still the outcome's: run_failed progress, outage progress, final
//!   completion and needs-input.
//! - P2: a receiver's `Retry-After` survives the hand-off to the outbox and
//!   is never shortened by the sender's backoff cap or `outbox run --once`.
//! - P2: the final event is persisted before progress still in delivery
//!   finishes, and events carry `eventSeq` for ordering.

mod common;

use common::*;
use serde_json::Value;
use std::fs;
use std::net::TcpListener;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

const T: &str = "mcp:launch:delivery";

fn mcp_env(mock: &MockMcp) -> Env {
    let env = Env::new();
    env.add_machine("mock", &mock.url, TOKEN);
    env
}

fn eventually(what: &str, timeout: Duration, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn outbox(env: &Env) -> Vec<Value> {
    let mut out: Vec<Value> = fs::read_dir(env.home.join("outbox"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_name().to_string_lossy().ends_with(".json") && !e.file_name().to_string_lossy().starts_with('.')
        })
        .map(|e| serde_json::from_str(&fs::read_to_string(e.path()).unwrap()).unwrap())
        .collect();
    out.sort_by_key(|e| e["createdMs"].as_u64());
    out
}

fn events(hook: &MockHook) -> Vec<String> {
    hook.requests()
        .iter()
        .map(|r| r.headers.get("x-t3-watch-event").cloned().unwrap_or_default())
        .collect()
}

/// Starts `t3-watch` with stdout and stderr connected to pipes whose read
/// ends are closed at once: every write to them fails with EPIPE.
fn spawn_closed(env: &Env, args: &[&str]) -> Child {
    let mut child = env
        .cmd(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    drop(child.stderr.take());
    child
}

fn exit_within(mut child: Child, timeout: Duration) -> i32 {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            return st.code().unwrap_or(-1);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("t3-watch did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---- P1: closed stdout and stderr ------------------------------------------

#[test]
fn closed_stdio_never_costs_a_followed_failure_or_the_final_event() {
    let mock = MockMcp::start();
    mock.set_thread(T, "failed", 5);
    let hook = MockHook::start();
    let env = mcp_env(&mock);
    let child = spawn_closed(
        &env,
        &[
            "wait",
            "--no-daemon",
            "--follow",
            "--durable",
            "--expect-min-run-ordinal",
            "5",
            "--webhook-url",
            &hook.url,
            T,
        ],
    );
    eventually("the run_failed webhook", Duration::from_secs(10), || {
        events(&hook) == ["run_failed"]
    });
    mock.set_thread(T, "completed", 6);
    assert_eq!(exit_within(child, Duration::from_secs(15)), 0, "not 101");
    assert_eq!(events(&hook), ["run_failed", "settled"]);
    assert!(outbox(&env).is_empty(), "both delivered, nothing queued");
}

#[test]
fn closed_stdio_with_the_receiver_down_still_persists_every_event() {
    let mock = MockMcp::start();
    mock.set_thread(T, "failed", 5);
    let env = mcp_env(&mock);
    let url = format!("http://127.0.0.1:{}/hooks/x", closed_port());
    let child = spawn_closed(
        &env,
        &[
            "wait",
            "--no-daemon",
            "--follow",
            "--durable",
            "--expect-min-run-ordinal",
            "5",
            "--webhook-url",
            &url,
            T,
        ],
    );
    eventually("the run_failed entry", Duration::from_secs(10), || {
        !outbox(&env).is_empty()
    });
    mock.set_thread(T, "completed", 6);
    assert_eq!(exit_within(child, Duration::from_secs(90)), 4);
    let entries = outbox(&env);
    let kinds: Vec<&str> = entries.iter().map(|e| e["event"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["run_failed", "settled"]);
    let seqs: Vec<u64> = entries
        .iter()
        .map(|e| {
            serde_json::from_str::<Value>(e["body"].as_str().unwrap()).unwrap()["eventSeq"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(seqs, [1, 2]);
}

#[test]
fn closed_stdio_never_costs_outage_progress() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 2);
    for _ in 0..2 {
        mock.with(|s| s.fail_queue.push_back((530, "error code: 1033".into())));
    }
    let hook = MockHook::start();
    let env = mcp_env(&mock);
    let child = spawn_closed(
        &env,
        &[
            "wait",
            "--no-daemon",
            "--durable",
            "--notify-unreachable",
            "0s",
            "--webhook-url",
            &hook.url,
            T,
        ],
    );
    assert_eq!(exit_within(child, Duration::from_secs(30)), 0);
    assert_eq!(events(&hook), ["upstream_unreachable", "upstream_restored", "settled"]);
}

#[test]
fn closed_stdio_never_costs_a_needs_input_wake() {
    let mock = MockMcp::start();
    mock.set_thread(T, "running", 2);
    mock.with(|s| {
        s.pending_count.insert(T.into(), 1);
    });
    let hook = MockHook::start();
    let env = mcp_env(&mock);
    // Through the daemon this time.
    let child = spawn_closed(&env, &["wait", "--durable", "--webhook-url", &hook.url, T]);
    assert_eq!(exit_within(child, Duration::from_secs(20)), 5);
    assert_eq!(events(&hook), ["needs_input"]);
}

// ---- P2: Retry-After is a floor --------------------------------------------

#[test]
fn a_receivers_retry_after_survives_the_handoff_and_the_cap() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let hook = MockHook::start();
    hook.push(429, &[("Retry-After", "3600")]);
    let env = mcp_env(&mock);
    let started_ms = now_ms();
    let out = env.run(&["wait", "--no-daemon", "--durable", "--webhook-url", &hook.url, T]);
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(hook.requests().len(), 1, "no inline retry inside the hour");
    let e = &outbox(&env)[0];
    assert!(e["notBeforeMs"].as_u64().unwrap() >= started_ms + 3_590_000, "{e}");
    assert!(e["nextAttemptMs"].as_u64().unwrap() >= started_ms + 3_590_000, "{e}");

    // Even a manual `outbox run --once` respects it.
    let run = env.run(&["outbox", "run", "--once"]);
    assert_eq!(run.status.code(), Some(4));
    assert_eq!(output_json(&run)["results"][0]["deferred"], true);
    assert_eq!(hook.requests().len(), 1);
}

#[test]
fn the_senders_backoff_cap_never_shortens_a_retry_after() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let hook = MockHook::start();
    for _ in 0..4 {
        hook.push(503, &[]);
    }
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--no-daemon", "--durable", "--webhook-url", &hook.url, T]);
    assert_eq!(out.status.code(), Some(4));
    // The sender's own retry gets a two-hour Retry-After: far beyond its
    // five-minute backoff cap.
    hook.push(429, &[("Retry-After", "7200")]);
    let before = now_ms();
    let run = env.run(&["outbox", "run", "--once"]);
    assert_eq!(run.status.code(), Some(4));
    let e = &outbox(&env)[0];
    assert!(e["nextAttemptMs"].as_u64().unwrap() >= before + 7_190_000, "{e}");
    assert!(e["notBeforeMs"].as_u64().unwrap() >= before + 7_190_000, "{e}");
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

// ---- P2: the final event does not wait behind progress ---------------------

#[test]
fn the_final_event_is_persisted_while_progress_is_still_being_delivered() {
    let mock = MockMcp::start();
    mock.set_thread(T, "failed", 5);
    // The receiver accepts and never answers: the run_failed delivery
    // blocks for its whole inline budget.
    let hook = MockHook::start();
    hook.set_mode(HookMode::Blackhole);
    let env = mcp_env(&mock);
    let mut child = env
        .cmd(&[
            "wait",
            "--no-daemon",
            "--follow",
            "--durable",
            "--expect-min-run-ordinal",
            "5",
            "--webhook-url",
            &hook.url,
            T,
        ])
        .spawn()
        .unwrap();
    eventually("the run_failed entry", Duration::from_secs(10), || {
        outbox(&env).len() == 1
    });
    mock.set_thread(T, "completed", 6);
    // The final event is on disk while the progress POST still hangs.
    eventually("the settled entry", Duration::from_secs(10), || outbox(&env).len() == 2);
    assert!(child.try_wait().unwrap().is_none(), "still delivering progress");
    child.kill().unwrap();
    child.wait().unwrap();

    hook.set_mode(HookMode::Normal);
    let run = env.run(&["outbox", "run", "--once"]);
    assert_eq!(run.status.code(), Some(0), "{}", String::from_utf8_lossy(&run.stdout));
    let mut got: Vec<(u64, String)> = hook
        .requests()
        .iter()
        .map(|r| {
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            (
                b["eventSeq"].as_u64().unwrap(),
                b["event"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    got.sort();
    assert_eq!(got, [(1, "run_failed".to_string()), (2, "settled".to_string())]);
}
