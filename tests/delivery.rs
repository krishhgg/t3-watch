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
use serde_json::{json, Value};
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
    let mut out: Vec<Value> = fs::read_dir(env.home.join(OUTBOX))
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

// ---- Sol ordinal 4, P2: webhook state does not wait for a slow hook --------

#[test]
fn retry_after_is_persisted_while_a_slow_hook_still_runs() {
    // Sol's repro, shortened: 429 Retry-After 3600 on the webhook while the
    // --on-settle hook is still sleeping.
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let hook = MockHook::start();
    hook.push(429, &[("Retry-After", "3600")]);
    let env = mcp_env(&mock);
    let started_ms = now_ms();
    let mut child = env
        .cmd(&[
            "wait",
            "--no-daemon",
            "--durable",
            "--webhook-url",
            &hook.url,
            "--on-settle",
            "/bin/sleep",
            "--on-settle-arg",
            "8",
            "--on-settle-timeout",
            "20s",
            T,
        ])
        .spawn()
        .unwrap();
    eventually("the Retry-After on disk", Duration::from_secs(5), || {
        outbox(&env)
            .first()
            .and_then(|e| e["notBeforeMs"].as_u64())
            .is_some_and(|t| t >= started_ms + 3_590_000)
    });
    assert!(child.try_wait().unwrap().is_none(), "the hook is still running");
    // A sender taking over now (as the daemon would) must keep the floor:
    // it finds the entry leased for the instant the client is still
    // recording, then deferred, never due.
    eventually("the sender to defer to Retry-After", Duration::from_secs(3), || {
        let run = env.run(&["outbox", "run", "--once"]);
        let r = &output_json(&run)["results"][0];
        assert!(r["deferred"] == true || r["inFlight"] == true, "{r}");
        r["deferred"] == true
    });
    assert!(child.try_wait().unwrap().is_none(), "the hook is still running");
    assert_eq!(hook.requests().len(), 1, "no second POST inside the hour");
    let st = child.wait().unwrap();
    assert_eq!(st.code(), Some(4));
    assert_eq!(hook.requests().len(), 1);
}

#[test]
fn the_sender_never_races_a_live_inline_delivery() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let hook = MockHook::start();
    hook.set_mode(HookMode::Blackhole);
    let env = mcp_env(&mock);
    let mut child = env
        .cmd(&["wait", "--no-daemon", "--durable", "--webhook-url", &hook.url, T])
        .spawn()
        .unwrap();
    eventually("the entry", Duration::from_secs(10), || outbox(&env).len() == 1);
    std::thread::sleep(Duration::from_millis(300));
    // The client is inside its inline POST: the sender must leave it alone,
    // even forced.
    let run = env.run(&["outbox", "run", "--once"]);
    let r = output_json(&run);
    assert_eq!(r["results"][0]["inFlight"], true, "{r}");
    assert_eq!(outbox(&env)[0]["attempts"], 0, "the sender touched a leased entry");
    child.kill().unwrap();
    child.wait().unwrap();
    // A dead client's lease is gone: the sender delivers.
    hook.set_mode(HookMode::Normal);
    let run = env.run(&["outbox", "run", "--once"]);
    assert_eq!(run.status.code(), Some(0), "{}", String::from_utf8_lossy(&run.stdout));
    assert_eq!(hook.requests().len(), 1);
}

// ---- diagnostics -----------------------------------------------------------

#[test]
fn results_and_the_trace_name_builds_and_delivery_timing() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 2);
    mock.with(|s| s.completed_at = Some("2026-11-07T07:00:11.387Z".into()));
    let hook = MockHook::start();
    let env = mcp_env(&mock);
    let out = env.run(&["wait", "--durable", "--webhook-url", &hook.url, T]);
    assert_eq!(out.status.code(), Some(0));
    let v = output_json(&out);
    assert_eq!(v["build"]["engine"], ENGINE);
    assert_eq!(v["build"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(v["daemon"]["engine"], ENGINE);
    assert!(v["daemon"]["pid"].as_u64().is_some());
    let t = &v["threads"][0];
    assert_eq!(t["runCompletedAt"], "2026-11-07T07:00:11.387Z");
    assert!(t["observedAt"].as_str().unwrap().ends_with('Z'));

    let watch_id = v["watchId"].as_str().unwrap();
    let trace = fs::read_to_string(env.home.join("trace.jsonl")).unwrap();
    let lines: Vec<Value> = trace.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let kinds: Vec<&str> = lines.iter().map(|l| l["kind"].as_str().unwrap()).collect();
    for k in [
        "wait.start",
        "daemon.start",
        "daemon.accepted",
        "watch.settled",
        "wait.outcome",
        "notify.prepared",
        "outbox.created",
        "notify.attempt",
        "notify.result",
        "wait.exit",
    ] {
        assert!(kinds.contains(&k), "{k} missing from {kinds:?}");
    }
    let pos = |k: &str| kinds.iter().position(|x| *x == k).unwrap();
    assert!(pos("wait.start") < pos("wait.outcome") && pos("wait.outcome") < pos("notify.attempt"));
    assert!(pos("notify.attempt") < pos("wait.exit"));
    let settled = &lines[pos("watch.settled")];
    assert_eq!(settled["runCompletedAt"], "2026-11-07T07:00:11.387Z");
    assert_eq!(lines[pos("wait.outcome")]["watchId"], watch_id);
    assert_eq!(lines[pos("notify.prepared")]["watchId"], watch_id);
    assert!(lines
        .iter()
        .all(|l| l["engine"] == ENGINE && l["at"].as_str().is_some()));
    // Ids and statuses only: nothing that could carry a secret.
    assert!(
        !trace.contains("secret-path-123"),
        "the webhook URL leaked into the trace"
    );
    let version = env.run(&["--version"]);
    assert!(String::from_utf8_lossy(&version.stdout).contains(&format!("engine {ENGINE}")));
}

// ---- Sol ordinal 5, P1: an expired final event is not "delivered" -----------

#[test]
fn a_final_event_that_expires_unsent_is_reported_undelivered() {
    // Sol's repro: --follow --durable --durable-max-age 1s, no hook. The
    // run_failed webhook's answer takes 7 s; meanwhile the run completes and
    // the final `settled` entry expires into dead letters, never sent.
    let mock = MockMcp::start();
    mock.set_thread(T, "failed", 5);
    let hook = MockHook::start();
    hook.state.lock().unwrap().delays.push_back(Duration::from_secs(7));
    let env = mcp_env(&mock);
    let child = env
        .cmd(&[
            "wait",
            "--follow",
            "--durable",
            "--durable-max-age",
            "1s",
            "--expect-min-run-ordinal",
            "5",
            "--webhook-url",
            &hook.url,
            T,
        ])
        .spawn()
        .unwrap();
    eventually("the run_failed webhook", Duration::from_secs(10), || {
        !hook.requests().is_empty()
    });
    mock.set_thread(T, "completed", 6);
    let (code, out, err) = finish(child, Duration::from_secs(60));
    let n = &out["notification"];
    assert_eq!(n["delivered"], false, "{out} {err}");
    assert_ne!(n["webhook"]["by"], "outbox sender", "absence taken as delivery: {n}");
    assert_eq!(n["webhook"]["disposition"], "expired", "{n}");
    assert_eq!(code, 4, "{out}");
    assert_eq!(events(&hook), ["run_failed"], "the final event was never sent");
    let dead = env
        .home
        .join(OUTBOX)
        .join("dead")
        .join(format!("{}.json", n["id"].as_str().unwrap()));
    assert!(dead.exists(), "the expired entry is kept as a dead letter");
    let receipt: Value = serde_json::from_str(
        &fs::read_to_string(
            env.home
                .join(OUTBOX)
                .join("receipts")
                .join(format!("{}.json", n["id"].as_str().unwrap())),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["disposition"], "expired");
}

// ---- Sol ordinal 5, P2: no secret from a thread URL in the trace -----------

#[test]
fn the_trace_never_holds_a_thread_urls_query() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let env = mcp_env(&mock);
    let url = format!(
        "{}/env-mock/{}?token=TRACE_QUERY_CANARY#frag-TRACE_QUERY_CANARY",
        mock.url,
        T.replace(':', "%3A")
    );
    let out = env.run(&["wait", "--no-daemon", &url]);
    assert_eq!(out.status.code(), Some(0));
    // An unparseable one, with --notify-on all so its error goes everywhere.
    let bad = format!("{}/?token=TRACE_QUERY_CANARY", mock.url);
    let hook = MockHook::start();
    let out2 = env.run(&[
        "wait",
        "--no-daemon",
        "--notify-on",
        "all",
        "--webhook-url",
        &hook.url,
        &bad,
    ]);
    assert_eq!(out2.status.code(), Some(2));
    let trace = fs::read_to_string(env.home.join("trace.jsonl")).unwrap();
    assert!(!trace.contains("TRACE_QUERY_CANARY"), "{trace}");
    assert!(trace.contains(&format!("\"threadId\":\"{T}\"")), "{trace}");
    for o in [&out, &out2] {
        assert!(!String::from_utf8_lossy(&o.stdout).contains("TRACE_QUERY_CANARY"));
    }
    assert!(hook
        .requests()
        .iter()
        .all(|r| !String::from_utf8_lossy(&r.body).contains("TRACE_QUERY_CANARY")));
}

// ---- Sol ordinal 5, P2: a FIFO where a log goes never blocks a wait --------

#[test]
fn a_fifo_at_the_trace_or_hook_log_never_blocks_a_wait() {
    for daemon in [false, true] {
        let mock = MockMcp::start();
        mock.set_thread(T, "completed", 1);
        let hook = MockHook::start();
        let env = mcp_env(&mock);
        for name in ["trace.jsonl", "hooks.log", "daemon.log"] {
            let st = std::process::Command::new("mkfifo")
                .arg(env.home.join(name))
                .status()
                .unwrap();
            assert!(st.success());
        }
        let started = Instant::now();
        let mut args = vec![
            "wait",
            "--timeout",
            "1s",
            "--webhook-url",
            &hook.url,
            "--on-settle",
            "/bin/sh",
            "--on-settle-arg",
            "-c",
            "--on-settle-arg",
            "echo hook-ran",
        ];
        if !daemon {
            args.insert(1, "--no-daemon");
        }
        args.push(T);
        let child = env.cmd(&args).spawn().unwrap();
        let code = exit_within(child, Duration::from_secs(10));
        assert_eq!(code, 0, "daemon={daemon}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "blocked {:?}",
            started.elapsed()
        );
        assert_eq!(events(&hook), ["settled"]);
    }
}

// ---- Sol ordinal 6 ---------------------------------------------------------

#[test]
fn the_inline_path_never_sends_an_entry_that_has_a_receipt() {
    // The final event's entry exists, but a receipt beside it says it was
    // dropped (as after a crash between the two writes). The client must not
    // send it, and must report the receipt's disposition.
    let mock = MockMcp::start();
    mock.set_thread(T, "failed", 5);
    let hook = MockHook::start();
    // The run_failed answer takes 5 s: the final entry waits meanwhile.
    hook.state.lock().unwrap().delays.push_back(Duration::from_secs(5));
    let env = mcp_env(&mock);
    let child = env
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
    eventually("the run_failed webhook", Duration::from_secs(10), || {
        !hook.requests().is_empty()
    });
    mock.set_thread(T, "completed", 6);
    let mut settled_id = None;
    eventually("the settled entry", Duration::from_secs(10), || {
        settled_id = outbox(&env)
            .iter()
            .find(|e| e["event"] == "settled")
            .map(|e| e["id"].as_str().unwrap().to_string());
        settled_id.is_some()
    });
    let id = settled_id.unwrap();
    let receipts = env.home.join(OUTBOX).join("receipts");
    fs::create_dir_all(&receipts).unwrap();
    let receipt = json!({ "id": id, "disposition": "dropped", "at": "2026-11-07T00:00:00.000Z",
                          "attempts": 0, "by": "outbox drop" });
    fs::write(receipts.join(format!("{id}.json")), receipt.to_string()).unwrap();
    let (code, out, err) = finish(child, Duration::from_secs(60));
    assert_eq!(code, 4, "{out} {err}");
    assert_eq!(out["notification"]["webhook"]["disposition"], "dropped", "{out}");
    assert_eq!(events(&hook), ["run_failed"], "the dropped event was sent");
    let kept: Value = serde_json::from_str(&fs::read_to_string(receipts.join(format!("{id}.json"))).unwrap()).unwrap();
    assert_eq!(kept, receipt, "the receipt was rewritten");
    assert!(outbox(&env).iter().all(|e| e["id"] != id.as_str()), "entry left behind");
}

#[test]
fn credentials_in_thread_urls_never_reach_output_trace_or_webhook() {
    let mock = MockMcp::start();
    mock.set_thread("parent", "completed", 1);
    let env = mcp_env(&mock);
    // Name the mock as the default machine so a t3-thread link resolves.
    let mut m = env.machines();
    m["default"] = json!("mock");
    fs::write(env.home.join("machines.json"), m.to_string()).unwrap();
    let hook = MockHook::start();
    for arg in [
        "t3-thread://v2/env/parent?token=QUERY_CANARY#FRAG_CANARY",
        "https://user:PASS_CANARY@relay.example/env/parent?token=QUERY_CANARY",
    ] {
        let out = env.run(&[
            "wait",
            "--no-daemon",
            "--notify-on",
            "all",
            "--webhook-url",
            &hook.url,
            arg,
        ]);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        for canary in ["QUERY_CANARY", "FRAG_CANARY", "PASS_CANARY"] {
            assert!(!text.contains(canary), "{arg}: {text}");
        }
        if arg.starts_with("https") {
            assert_eq!(out.status.code(), Some(2), "{text}");
            assert!(text.contains("must not carry credentials"), "{text}");
        } else {
            assert_eq!(out.status.code(), Some(0), "{text}");
            assert_eq!(output_json(&out)["threads"][0]["threadId"], "parent");
        }
    }
    let trace = fs::read_to_string(env.home.join("trace.jsonl")).unwrap();
    for canary in ["QUERY_CANARY", "FRAG_CANARY", "PASS_CANARY"] {
        assert!(!trace.contains(canary), "{trace}");
        assert!(hook
            .requests()
            .iter()
            .all(|r| !String::from_utf8_lossy(&r.body).contains(canary)));
    }
}

#[test]
fn the_timeline_judges_the_final_event_not_an_earlier_progress_one() {
    // Sol's case: the run_failed progress webhook gets a 200, the final
    // settled one a 401. The acceptance event was not delivered.
    let mock = MockMcp::start();
    mock.set_thread(T, "failed", 5);
    let hook = MockHook::start();
    hook.push(200, &[]);
    hook.push(401, &[]);
    let env = mcp_env(&mock);
    let child = env
        .cmd(&[
            "wait",
            "--no-daemon",
            "--follow",
            "--expect-min-run-ordinal",
            "5",
            "--webhook-url",
            &hook.url,
            T,
        ])
        .spawn()
        .unwrap();
    eventually("the run_failed webhook", Duration::from_secs(10), || {
        !hook.requests().is_empty()
    });
    mock.set_thread(T, "completed", 6);
    let (code, out, _) = finish(child, Duration::from_secs(30));
    assert_eq!(code, 4, "{out}");
    let tl = std::process::Command::new("python3")
        .args(["-I", concat!(env!("CARGO_MANIFEST_DIR"), "/e2e/timeline.py")])
        .arg(env.home.join("trace.jsonl"))
        .output()
        .unwrap();
    assert!(tl.status.success(), "{}", String::from_utf8_lossy(&tl.stderr));
    let stdout = String::from_utf8_lossy(&tl.stdout);
    let summary: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(summary["acceptanceEvent"]["event"], "settled", "{summary}");
    assert_eq!(summary["acceptanceDelivered"], false, "{summary}");
    assert_eq!(summary["completionTo2xxS"], Value::Null, "{summary}");
    assert_eq!(summary["exitCode"], 4);
    let progress = summary["progressEvents"].as_array().unwrap();
    assert_eq!(progress.len(), 1);
    assert_eq!(progress[0]["event"], "run_failed");
    assert!(progress[0]["deliveredAt"].as_str().is_some(), "{summary}");
}

#[test]
fn the_timeline_never_takes_a_progress_event_for_a_final_outcome_without_one() {
    // Sol ordinal 7: run_failed went out (200), then the wait timed out
    // under the default --notify-on, which prepares no timeout event. The
    // delivered run_failed must not be read as the acceptance event.
    for notify_on in [None, Some("all")] {
        let mock = MockMcp::start();
        mock.set_thread(T, "failed", 5);
        let hook = MockHook::start();
        let env = mcp_env(&mock);
        let mut args = vec![
            "wait",
            "--no-daemon",
            "--follow",
            "--expect-min-run-ordinal",
            "5",
            "--timeout",
            "3s",
            "--webhook-url",
            &hook.url,
        ];
        if let Some(n) = notify_on {
            args.extend(["--notify-on", n]);
        }
        args.push(T);
        let out = env.run(&args);
        assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stdout));
        let tl = std::process::Command::new("python3")
            .args(["-I", concat!(env!("CARGO_MANIFEST_DIR"), "/e2e/timeline.py")])
            .arg(env.home.join("trace.jsonl"))
            .output()
            .unwrap();
        assert!(tl.status.success(), "{}", String::from_utf8_lossy(&tl.stderr));
        let stdout = String::from_utf8_lossy(&tl.stdout);
        let summary: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
        assert_eq!(summary["acceptanceEvent"]["event"], "timeout", "{summary}");
        let progress = summary["progressEvents"].as_array().unwrap();
        assert!(
            progress
                .iter()
                .any(|p| p["event"] == "run_failed" && p["deliveredAt"].is_string()),
            "{summary}"
        );
        if notify_on.is_none() {
            assert_eq!(summary["acceptanceDelivered"], false, "{summary}");
            assert_eq!(summary["acceptanceEvent"]["id"], Value::Null, "{summary}");
            assert!(
                summary["acceptanceEvent"]["missing"]
                    .as_str()
                    .is_some_and(|m| m.contains("timeout")),
                "{summary}"
            );
            assert_eq!(summary["completionTo2xxS"], Value::Null, "{summary}");
        } else {
            // With --notify-on all the bot hears about the timeout too.
            assert_eq!(summary["acceptanceDelivered"], true, "{summary}");
            assert!(summary["acceptanceEvent"]["id"].is_string(), "{summary}");
            assert_eq!(summary["acceptanceEvent"]["missing"], Value::Null, "{summary}");
            assert!(hook
                .requests()
                .iter()
                .any(|r| r.headers.get("x-t3-watch-event").map(String::as_str) == Some("timeout")));
        }
    }
}
