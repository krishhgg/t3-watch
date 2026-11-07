//! Regressions for the fourth review (Sol, ordinal 4), P1: a new client
//! must never trust a daemon of another engine revision, and an old build's
//! outbox sender must never handle this build's entries.
//!
//! The old daemons here are fakes speaking the old protocol exactly: an
//! `accepted` line without an engine (or with another one), then a
//! `settled`/`completed` answer, which is what a 53d5956 or 6bd6c1e daemon
//! sends while its engine has known safety bugs.

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const T: &str = "mcp:launch:compat";

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

/// A daemon of another build: answers every request with `lines`.
fn fake_daemon(sock: &Path, lines: Vec<Value>) -> Arc<AtomicUsize> {
    let _ = fs::remove_file(sock);
    let listener = UnixListener::bind(sock).unwrap();
    fs::set_permissions(sock, fs::Permissions::from_mode(0o600)).unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    let count = served.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            count.fetch_add(1, Ordering::SeqCst);
            let lines = lines.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(conn.try_clone().unwrap());
                let mut req = String::new();
                let _ = r.read_line(&mut req);
                let mut w = conn;
                for l in &lines {
                    let _ = writeln!(w, "{l}");
                }
                std::thread::sleep(Duration::from_millis(200));
            });
        }
    });
    served
}

fn completed_by_old_engine(accepted: Value) -> Vec<Value> {
    vec![
        accepted,
        json!({ "event": "settled", "outcome": "completed", "mode": "all", "via": "daemon", "elapsedMs": 5,
                "threads": [{ "threadId": T, "status": "completed", "outcome": "completed", "settled": true,
                              "runOrdinal": 2 }] }),
    ]
}

fn private_dir(env: &Env, name: &str) -> PathBuf {
    let d = env.home.join(name);
    fs::create_dir_all(&d).unwrap();
    fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
    d
}

#[test]
fn a_daemon_of_another_engine_on_a_custom_socket_is_refused() {
    let accepted_lines = [
        // 53d5956 and 6bd6c1e: no engine at all, version 0.1.0.
        json!({ "event": "accepted", "pid": 4242, "version": "0.1.0" }),
        // A future or past engine.
        json!({ "event": "accepted", "pid": 4242, "version": "0.2.0", "engine": 3, "rev": "old" }),
    ];
    for accepted in accepted_lines {
        let mock = MockMcp::start();
        mock.set_thread(T, "running", 3);
        let hook = MockHook::start();
        let env = mcp_env(&mock);
        let sock = private_dir(&env, "s").join("shared.sock");
        let served = fake_daemon(&sock, completed_by_old_engine(accepted.clone()));
        let out = env
            .cmd(&[
                "wait",
                "--notify-on",
                "settled,needs_input",
                "--webhook-url",
                &hook.url,
                T,
            ])
            .env("T3_WATCH_SOCKET", &sock)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{accepted}: {}", both(&out));
        let v = output_json(&out);
        assert_eq!(v["error"]["kind"], "incompatible_daemon", "{v}");
        assert!(v["error"]["message"].as_str().unwrap().contains("engine"));
        assert_ne!(v["outcome"], "completed");
        assert_eq!(served.load(Ordering::SeqCst), 1);
        assert!(
            hook.requests().is_empty(),
            "nothing may be announced from an old engine"
        );
        let trace = fs::read_to_string(env.home.join("trace.jsonl")).unwrap();
        assert!(trace.contains("\"kind\":\"daemon.incompatible\""), "{trace}");
    }
}

#[test]
fn an_answer_without_any_accepted_line_is_not_trusted() {
    let mock = MockMcp::start();
    mock.set_thread(T, "running", 3);
    let env = mcp_env(&mock);
    let sock = private_dir(&env, "s").join("shared.sock");
    let mut lines = completed_by_old_engine(json!(null));
    lines.remove(0);
    fake_daemon(&sock, lines);
    let out = env.cmd(&["wait", T]).env("T3_WATCH_SOCKET", &sock).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", both(&out));
    assert_eq!(output_json(&out)["error"]["kind"], "incompatible_daemon");
}

#[test]
fn an_old_daemon_at_the_old_default_socket_is_never_reached() {
    let mock = MockMcp::start();
    mock.set_thread(T, "running", 3);
    let env = mcp_env(&mock);
    // A 6bd6c1e daemon would listen here and answer "completed".
    fs::set_permissions(&env.home, fs::Permissions::from_mode(0o700)).unwrap();
    let old = env.home.join("daemon-p2.sock");
    let served = fake_daemon(
        &old,
        completed_by_old_engine(json!({ "event": "accepted", "pid": 4242, "version": "0.1.0" })),
    );
    let out = env.run(&["wait", "--timeout", "1500ms", T]);
    // This build's own daemon answered: the thread still runs.
    assert_eq!(out.status.code(), Some(3), "{}", both(&out));
    let v = output_json(&out);
    assert_eq!(v["daemon"]["engine"], 4, "{v}");
    assert_eq!(v["build"]["engine"], 4);
    assert_eq!(served.load(Ordering::SeqCst), 0, "the old socket was contacted");
}

#[test]
fn this_daemon_refuses_a_request_from_another_engine() {
    let env = Env::new();
    let mut daemon = env.spawn(&["daemon", "--idle-exit", "30s"]);
    env.wait_status("daemon", Duration::from_secs(10), |s| s["running"] == true);
    assert_eq!(env.status()["build"]["engine"], 4);
    // The request a 6bd6c1e client sends: no engine field.
    let old_request = json!({ "op": "wait", "targets": [{ "transport": "mcp", "machine": "m", "threadId": T }],
                              "mode": "all", "timeoutMs": 1000 });
    for req in [old_request.clone(), {
        let mut r = old_request.clone();
        r["engine"] = json!(3);
        r
    }] {
        let mut s = UnixStream::connect(env.home.join(SOCK)).unwrap();
        writeln!(s, "{req}").unwrap();
        let mut line = String::new();
        BufReader::new(&s).read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["error"]["kind"], "incompatible_client", "{v}");
    }
    env.run(&["stop"]);
    daemon.wait().unwrap();
}

// ---- old outbox senders --------------------------------------------------

fn legacy_entry(id: &str, url: &str, created_ms: u64) -> Value {
    json!({ "version": 1, "id": id, "event": "settled",
            "body": json!({"event": "settled", "outcome": "completed"}).to_string(),
            "url": url, "headers": [], "secret": null,
            "createdMs": created_ms, "expiresMs": created_ms + 86_400_000,
            "attempts": 4, "retries": 0, "nextAttemptMs": created_ms + 2_000 })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn entries_left_by_an_older_build_are_migrated_only_when_no_client_can_own_them() {
    let env = Env::new();
    let hook = MockHook::start();
    let legacy = private_dir(&env, "outbox");
    let (old_id, young_id) = ("00aa11bb", "00cc22dd");
    fs::write(
        legacy.join(format!("{old_id}.json")),
        legacy_entry(old_id, &hook.url, now_ms() - 600_000).to_string(),
    )
    .unwrap();
    fs::write(
        legacy.join(format!("{young_id}.json")),
        legacy_entry(young_id, &hook.url, now_ms()).to_string(),
    )
    .unwrap();
    let run = env.run(&["outbox", "run", "--once"]);
    let r = output_json(&run);
    assert!(r["results"].to_string().contains("\"migrated\":true"), "{r}");
    // The old entry moved to outbox-v2 and was delivered with its own id.
    let reqs = hook.requests();
    assert_eq!(reqs.len(), 1, "{r}");
    assert_eq!(reqs[0].headers["x-t3-watch-id"], old_id);
    assert!(!legacy.join(format!("{old_id}.json")).exists());
    // A young one may still belong to a running client of that build.
    assert!(legacy.join(format!("{young_id}.json")).exists());
}

#[test]
fn this_builds_entries_live_where_old_senders_never_look() {
    let mock = MockMcp::start();
    mock.set_thread(T, "completed", 1);
    let env = mcp_env(&mock);
    let url = format!(
        "http://127.0.0.1:{}/hooks/x",
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    );
    let out = env.run(&["wait", "--no-daemon", "--durable", "--webhook-url", &url, T]);
    assert_eq!(out.status.code(), Some(4), "{}", both(&out));
    assert!(!env.home.join("outbox").exists(), "old senders read outbox/");
    let v2: Vec<_> = fs::read_dir(env.home.join("outbox-v2"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
        .collect();
    assert_eq!(v2.len(), 1);
}

#[test]
fn a_young_entry_left_by_a_crashed_old_client_is_kept_pending_and_delivered() {
    // Sol's repro: a valid legacy entry too young to migrate, no old client
    // or sender alive. It must count as pending everywhere, keep the daemon
    // up past its idle exit, and go out once eligible. Created 80 s ago so
    // it becomes eligible 10 s into the test (the grace is 90 s).
    let env = Env::new();
    let hook = MockHook::start();
    let legacy = private_dir(&env, "outbox");
    let id = "00ee33ff";
    fs::write(
        legacy.join(format!("{id}.json")),
        legacy_entry(id, &hook.url, now_ms() - 80_000).to_string(),
    )
    .unwrap();

    let run = env.run(&["outbox", "run", "--once"]);
    assert_eq!(run.status.code(), Some(4), "a deferred entry is not an empty outbox");
    let r = output_json(&run);
    assert!(r["pending"].as_u64().unwrap() >= 1, "{r}");
    assert!(r["results"].to_string().contains("\"legacy\":true"), "{r}");
    let list = output_json(&env.run(&["outbox", "list"]));
    assert_eq!(list["legacy"][0]["id"], id, "{list}");

    // The daemon (idle exit 2 s in tests) stays up for it.
    let mut daemon = env.spawn(&["daemon"]);
    env.wait_status("daemon", Duration::from_secs(10), |s| s["running"] == true);
    assert_eq!(env.status()["outbox"]["legacyPending"], 1);
    std::thread::sleep(Duration::from_secs(5));
    assert!(
        daemon.try_wait().unwrap().is_none(),
        "the daemon left a legacy entry stranded"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while hook.requests().is_empty() {
        assert!(std::time::Instant::now() < deadline, "never delivered");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(hook.requests()[0].headers["x-t3-watch-id"], id);
    // Delivered and nothing left: now it may idle out.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while daemon.try_wait().unwrap().is_none() {
        assert!(std::time::Instant::now() < deadline, "the daemon never idled out");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(!legacy.join(format!("{id}.json")).exists());
    let receipt = fs::read_to_string(env.home.join("outbox-v2").join("receipts").join(format!("{id}.json"))).unwrap();
    assert!(receipt.contains("\"disposition\":\"delivered\""), "{receipt}");
}
