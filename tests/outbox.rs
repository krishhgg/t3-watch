//! Durable notifications (`--durable`): the legacy watcher timed out, then
//! lost its only notification to a DNS failure with no retry. Here the
//! notification is persisted before the first attempt, retried by a
//! supervised sender until a 2xx, and carries one stable id, so:
//!
//! - a DNS failure leaves it queued (and private), not lost;
//! - a receiver that is down, then up, gets it from the outbox;
//! - a crash mid-delivery loses nothing;
//! - an answer lost after the receiver accepted produces duplicates with
//!   the same id, never a different event;
//! - the daemon keeps retrying (and stays up) until delivery, including a
//!   needs-input wake while the run still says `running`.

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::net::TcpListener;
use std::time::{Duration, Instant};

const T: &str = "mcp:launch:outbox";

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
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
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A port nothing listens on (yet).
fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn outbox_files(env: &Env) -> Vec<String> {
    fs::read_dir(env.home.join("outbox"))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".json") && !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default()
}

fn durable_wait(env: &Env, url: &str, extra: &[&str]) -> std::process::Command {
    let mut args = vec![
        "wait",
        "--no-daemon",
        "--durable",
        "--webhook-url",
        url,
        "--webhook-secret-env",
        "OB_SECRET",
        "--webhook-header-env",
        "Authorization=OB_AUTH",
    ];
    args.extend_from_slice(extra);
    args.push(T);
    let mut c = env.cmd(&args);
    c.env("OB_SECRET", "outbox-signing-secret")
        .env("OB_AUTH", "Bearer outbox-auth-value");
    c
}

fn signed_ok(r: &HookReq) -> bool {
    let h = &r.headers;
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"outbox-signing-secret");
    let mut data = format!("{}.{}.", h["x-t3-watch-timestamp"], h["x-t3-watch-id"]).into_bytes();
    data.extend_from_slice(&r.body);
    h["x-t3-watch-signature"] == format!("sha256={}", hex(ring::hmac::sign(&key, &data).as_ref()))
        && h["authorization"] == "Bearer outbox-auth-value"
        && h["idempotency-key"] == h["x-t3-watch-id"]
}

#[test]
fn a_dns_failure_is_queued_privately_not_lost() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let env = mcp_env(&mock);
    let out = durable_wait(&env, "https://t3w-outbox-test.invalid/hooks/secret-path-9", &[])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "{}", both(&out));
    let v = output_json(&out);
    assert_eq!(v["outcome"], "completed");
    let n = &v["notification"];
    assert_eq!(n["delivered"], false);
    assert_eq!(n["queued"], true);
    let id = n["webhook"]["outboxId"].as_str().unwrap().to_string();
    assert_eq!(id, n["id"].as_str().unwrap());

    // Kept, private, and listed without secrets.
    assert_eq!(outbox_files(&env), [format!("{id}.json")]);
    assert_eq!(mode(&env.home.join("outbox")), 0o700);
    assert_eq!(mode(&env.home.join("outbox").join(format!("{id}.json"))), 0o600);
    let list = env.run(&["outbox", "list"]);
    assert_eq!(list.status.code(), Some(0));
    let text = String::from_utf8_lossy(&list.stdout).to_string();
    let l: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(l["entries"][0]["id"], id);
    assert_eq!(l["entries"][0]["destination"], "https://t3w-outbox-test.invalid");
    assert!(l["entries"][0]["attempts"].as_u64().unwrap() >= 1);
    for secret in ["secret-path-9", "outbox-signing-secret", "outbox-auth-value"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
        assert!(!both(&out).contains(secret), "{secret} leaked in the wait output");
    }

    let dropped = env.run(&["outbox", "drop", &id]);
    assert_eq!(dropped.status.code(), Some(0));
    assert!(outbox_files(&env).is_empty());
}

#[test]
fn a_receiver_that_comes_back_gets_the_queued_event_with_the_same_id() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let env = mcp_env(&mock);
    let port = closed_port();
    let url = format!("http://127.0.0.1:{port}/hooks/secret-path-123");
    let out = durable_wait(&env, &url, &[]).output().unwrap();
    assert_eq!(out.status.code(), Some(4), "{}", both(&out));
    let id = output_json(&out)["notification"]["id"].as_str().unwrap().to_string();

    let hook = MockHook::start_on(port);
    let run = env.run(&["outbox", "run", "--once"]);
    assert_eq!(run.status.code(), Some(0), "{}", both(&run));
    let r = output_json(&run);
    assert_eq!(r["results"][0]["delivered"], true);
    assert_eq!(r["pending"], 0);
    let reqs = hook.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].headers["x-t3-watch-id"], id);
    assert!(signed_ok(&reqs[0]));
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["event"], "settled");
    assert_eq!(body["outcome"], "completed");
    assert!(outbox_files(&env).is_empty());
}

#[test]
fn a_crash_during_delivery_loses_nothing() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let env = mcp_env(&mock);
    // The receiver accepts connections but never reads them: the request is
    // never processed, and the client blocks waiting for an answer.
    let hook = MockHook::start();
    hook.set_mode(HookMode::Blackhole);
    let mut child = durable_wait(&env, &hook.url, &[]).spawn().unwrap();
    eventually("the outbox entry", secs(10), || outbox_files(&env).len() == 1);
    std::thread::sleep(Duration::from_millis(300));
    child.kill().unwrap(); // SIGKILL: no cleanup runs
    child.wait().unwrap();
    assert_eq!(outbox_files(&env).len(), 1, "the entry survives the crash");
    assert!(hook.requests().is_empty(), "nothing was processed before the crash");

    hook.set_mode(HookMode::Normal);
    let run = env.run(&["outbox", "run", "--once"]);
    assert_eq!(run.status.code(), Some(0), "{}", both(&run));
    let reqs = hook.requests();
    assert_eq!(reqs.len(), 1);
    assert!(signed_ok(&reqs[0]));
    let id = outbox_id(&run);
    assert_eq!(reqs[0].headers["x-t3-watch-id"], id);
    assert!(outbox_files(&env).is_empty());
}

fn outbox_id(run: &std::process::Output) -> String {
    output_json(run)["results"][0]["id"].as_str().unwrap().to_string()
}

#[test]
fn an_answer_lost_after_acceptance_is_resent_with_the_same_id() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let env = mcp_env(&mock);
    // The receiver records each request, then drops the connection: it has
    // the event, but the sender never hears so.
    let hook = MockHook::start();
    hook.set_mode(HookMode::DropAfterRecord);
    let out = durable_wait(&env, &hook.url, &[]).output().unwrap();
    assert_eq!(out.status.code(), Some(4), "{}", both(&out));
    let id = output_json(&out)["notification"]["id"].as_str().unwrap().to_string();
    let inline = hook.requests().len();
    assert!(inline >= 2, "inline retries: {inline}");

    hook.set_mode(HookMode::Normal);
    let run = env.run(&["outbox", "run", "--once"]);
    assert_eq!(run.status.code(), Some(0), "{}", both(&run));
    let reqs = hook.requests();
    assert_eq!(reqs.len(), inline + 1);
    // Every copy is the same event: same id, same idempotency key, same
    // body; each signed for its own attempt.
    let first_body = &reqs[0].body;
    for (i, r) in reqs.iter().enumerate() {
        assert_eq!(r.headers["x-t3-watch-id"], id);
        assert_eq!(r.headers["idempotency-key"], id);
        assert_eq!(&r.body, first_body);
        assert!(signed_ok(r));
        assert_eq!(r.headers["x-t3-watch-attempt"], (i + 1).to_string());
    }
}

#[test]
fn an_expired_entry_moves_to_dead_letters() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let env = mcp_env(&mock);
    let url = format!("http://127.0.0.1:{}/hooks/x", closed_port());
    let out = durable_wait(&env, &url, &["--durable-max-age", "1s"]).output().unwrap();
    assert_eq!(out.status.code(), Some(4), "{}", both(&out));
    let id = output_json(&out)["notification"]["id"].as_str().unwrap().to_string();
    std::thread::sleep(Duration::from_millis(1200));
    let run = env.run(&["outbox", "run", "--once"]);
    let r = output_json(&run);
    assert_eq!(r["results"][0]["deadLettered"], true, "{r}");
    assert!(outbox_files(&env).is_empty());
    assert!(env.home.join("outbox").join("dead").join(format!("{id}.json")).exists());
}

#[test]
fn the_daemon_keeps_retrying_a_needs_input_wake_until_delivered() {
    let mock = MockMcp::start();
    // Blocked on a question while the run still says `running`.
    mock.set_thread(T, "running", 2);
    mock.with(|s| {
        s.pending_count.insert(T.into(), 1);
    });
    let env = mcp_env(&mock);
    let port = closed_port();
    let url = format!("http://127.0.0.1:{port}/hooks/secret-path-123");
    let out = env
        .cmd(&["wait", "--durable", "--slice", "300ms", "--webhook-url", &url, T])
        .env("T3_WATCH_IDLE_EXIT", "1s")
        .output()
        .unwrap();
    // Needs input keeps its own exit code; the notification is queued.
    assert_eq!(out.status.code(), Some(5), "{}", both(&out));
    let v = output_json(&out);
    assert_eq!(v["event"], "needs_input");
    assert_eq!(v["notification"]["queued"], true);
    let id = v["notification"]["id"].as_str().unwrap().to_string();

    // Longer than the idle exit: the daemon stays up for the outbox.
    std::thread::sleep(secs(3));
    let st = env.status();
    assert_eq!(st["running"], true, "{st}");
    assert_eq!(st["outbox"]["pending"], 1);

    let hook = MockHook::start_on(port);
    eventually("the daemon to deliver the queued event", secs(20), || {
        !hook.requests().is_empty()
    });
    let r = &hook.requests()[0];
    assert_eq!(r.headers["x-t3-watch-id"], id);
    assert_eq!(r.headers["x-t3-watch-event"], "needs_input");
    let body: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(body["threads"][0]["pendingRequestCount"], 1);
    eventually("the outbox to empty", secs(10), || outbox_files(&env).is_empty());
    // With nothing left to send, the daemon idles out.
    eventually("the daemon to idle out", secs(15), || {
        env.status()["running"] != json!(true)
    });
}
